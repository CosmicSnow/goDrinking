//! Thin Tauri shell over `golive-core`.
//!
//! Rules enforced here:
//! - No product logic: commands validate input, drive the core, and return.
//!   Sala/video decisions live in the core; orchestration between signal and
//!   media lives in [`pump`] (a future `core::runtime` candidate).
//! - Core events become Tauri events (`signal-event`, `media-event`) for the
//!   future UI. Commands never poll.
//! - Errors are redacted Strings. Passwords, tokens and SDP/candidates never
//!   reach logs or the frontend (envelopes carry kinds only).
//! - Locks are held briefly; never across `.await` (media sessions sit
//!   behind `tokio::sync::Mutex`, the sync core behind short std locks).

pub mod audio;
pub mod player;
pub mod pump;
pub mod screen;
pub mod session_log;
pub mod video;

use golive_core::media::{
    EngineKind, ExternalFrame, ExternalSource, MediaEvent, NativeViewer, Publisher, Quality,
    QualityProfile, VideoSource,
};
use golive_core::owner::{Fence, Owner, OwnerSnapshot};
use golive_core::signal::SignalClient;
use golive_platform::AudioApp;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

pub const DEFAULT_SERVER: &str = "https://together.jouymaker.com";

struct StartupCancelGuard {
    cancel: Arc<AtomicBool>,
    armed: bool,
}

impl StartupCancelGuard {
    fn new(cancel: Arc<AtomicBool>) -> Self {
        Self {
            cancel,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartupCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancel.store(true, Ordering::Release);
        }
    }
}

async fn start_capture_for_blocking(
    kind: golive_platform::SourceKind,
    id: String,
    profile: QualityProfile,
    live: Arc<Mutex<QualityProfile>>,
    cancel: Arc<AtomicBool>,
) -> Result<
    (
        std::sync::mpsc::Receiver<ExternalFrame>,
        screen::BridgeHandle,
        String,
    ),
    golive_platform::PlatformError,
> {
    tokio::task::spawn_blocking(move || {
        screen::start_capture_for_with_cancel(kind, &id, profile, live, cancel)
    })
    .await
    .map_err(|_| {
        golive_platform::PlatformError::Internal("inicialização de captura cancelada".into())
    })?
}

async fn start_capture_combo_blocking(
    screen_kind: golive_platform::SourceKind,
    screen_id: String,
    camera_id: String,
    profile: QualityProfile,
    live: Arc<Mutex<QualityProfile>>,
    cancel: Arc<AtomicBool>,
) -> Result<
    (
        std::sync::mpsc::Receiver<ExternalFrame>,
        screen::BridgeHandle,
        String,
    ),
    golive_platform::PlatformError,
> {
    tokio::task::spawn_blocking(move || {
        screen::start_capture_combo_with_cancel(
            screen_kind,
            &screen_id,
            &camera_id,
            profile,
            live,
            cancel,
        )
    })
    .await
    .map_err(|_| {
        golive_platform::PlatformError::Internal("inicialização de captura cancelada".into())
    })?
}

/// Share source selector. Screen capture arrives via the platform bridge
/// (`display:<id>` / `window:<id>`); synthetic + movie stay untouched.
/// `camera:<id>` streams a webcam alone; `combo:display:<id>+camera:<cid>`
/// (or `combo:window:…`) composites screen + webcam corner overlay into one
/// feed (single publisher, protocol-unchanged).
#[derive(Clone, Debug)]
pub enum ShareSource {
    Synthetic,
    Movie(String),
    Display(String),
    Window(String),
    Camera(String),
    Combo {
        screen: Box<ShareSource>,
        camera: String,
    },
}

pub(crate) fn initial_share_profile(
    profile: Option<QualityProfile>,
) -> Result<QualityProfile, String> {
    match profile {
        Some(profile) => {
            profile.validate().map_err(|e| format!("qualidade: {e}"))?;
            Ok(profile)
        }
        None => Ok(Quality::P720.profile()),
    }
}

pub(crate) fn window_audio_id(source: &ShareSource) -> Option<&str> {
    match source {
        ShareSource::Window(id) => Some(id.as_str()),
        ShareSource::Combo { screen, .. } => window_audio_id(screen),
        _ => None,
    }
}

impl ShareSource {
    fn parse(raw: &str) -> Result<Self, String> {
        if raw == "synthetic" {
            Ok(Self::Synthetic)
        } else if let Some(path) = raw.strip_prefix("movie:") {
            if path.is_empty() {
                Err("movie: needs a path".into())
            } else {
                Ok(Self::Movie(path.to_owned()))
            }
        } else if let Some(id) = raw.strip_prefix("display:") {
            if id.trim().is_empty() {
                Err("display: needs an id (list sources first)".into())
            } else {
                Ok(Self::Display(id.trim().to_owned()))
            }
        } else if let Some(id) = raw.strip_prefix("window:") {
            if id.trim().is_empty() {
                Err("window: needs an id (list sources first)".into())
            } else {
                Ok(Self::Window(id.trim().to_owned()))
            }
        } else if let Some(id) = raw.strip_prefix("camera:") {
            if id.trim().is_empty() {
                Err("camera: needs an id (list sources first)".into())
            } else {
                Ok(Self::Camera(id.trim().to_owned()))
            }
        } else if let Some(rest) = raw.strip_prefix("combo:") {
            Self::parse_combo(rest)
        } else {
            Err("fonte: 'synthetic', 'movie:/caminho', 'display:<id>', 'window:<id>', 'camera:<id>' ou 'combo:display:<id>+camera:<cid>'".into())
        }
    }

    /// Parses `display:<sid>+camera:<cid>` / `window:<sid>+camera:<cid>`.
    /// The screen half reuses the single-source rules (OS handles never
    /// contain `+camera:`); anything else is a typed error, never a guess.
    fn parse_combo(rest: &str) -> Result<Self, String> {
        let (screen_part, camera_id) = rest.split_once("+camera:").ok_or_else(|| {
            "combo: use 'combo:display:<id>+camera:<cid>' ou 'combo:window:<id>+camera:<cid>'"
                .to_owned()
        })?;
        if camera_id.trim().is_empty() {
            return Err("combo: camera: needs an id (list sources first)".into());
        }
        let screen = if let Some(id) = screen_part.strip_prefix("display:") {
            if id.trim().is_empty() {
                return Err("combo: display: needs an id (list sources first)".into());
            }
            Self::Display(id.trim().to_owned())
        } else if let Some(id) = screen_part.strip_prefix("window:") {
            if id.trim().is_empty() {
                return Err("combo: window: needs an id (list sources first)".into());
            }
            Self::Window(id.trim().to_owned())
        } else {
            return Err(
                "combo: a tela é 'display:<id>' ou 'window:<id>' (synthetic/movie/camera não combinam)"
                    .into(),
            );
        };
        Ok(Self::Combo {
            screen: Box::new(screen),
            camera: camera_id.trim().to_owned(),
        })
    }
}

/// `set_quality` payload (UI lane builds it via `resolveDesired`).
/// Numerics are authoritative; `preset`, when present, must name a known
/// preset (`low|medium|high`) and is echoed for display only.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct SetQualityArgs {
    pub w: u32,
    pub h: u32,
    /// Accepts the canonical snake_case and the legacy camelCase spelling
    /// (older packaged frontends send `bitrateKbps`); both bind here so a
    /// stale UI never fails deserialization against a new backend.
    #[serde(alias = "bitrateKbps")]
    pub bitrate_kbps: u32,
    pub fps: u32,
    #[serde(default)]
    pub preset: Option<String>,
}

/// Redacted media counters observed by forward tasks (pollable fallback
/// next to push events; kinds and counts only).
#[derive(Clone, Default, Debug, serde::Serialize)]
pub struct MediaCounters {
    pub connected: bool,
    pub frames: u64,
    pub keyframes: u64,
    pub keyframes_seen: bool,
    /// Frames actually presented in native windows (acks from helpers).
    /// Distinct from `frames` (decoded): this is the presentation evidence.
    pub presented: u64,
    /// Per-link transmission stats (one entry per watched member with a
    /// native window). Consumed by the future UI lane; empty when idle.
    #[serde(default)]
    pub links: Vec<video::LinkStats>,
    /// Effective share quality (authoritative snapshot for the UI lane).
    /// `None` when not sharing. Generation bumps async via `media-event`.
    #[serde(default)]
    pub effective: Option<EffectiveQuality>,
    /// Live encode backend (`videotoolbox`/`nvenc`/`openh264`) for the UI badge
    /// and diagnostics. First publisher reporting wins; `None` until the
    /// encode thread finishes its first build. Read on snapshot/event —
    /// never polled.
    #[serde(default)]
    pub backend: Option<String>,
    /// Why the software fallback, when `backend` is `openh264`.
    /// Triaged in-app (deterministic, no probe access needed): test hook,
    /// platform without VideoToolbox, or a failed probe (details in the
    /// session log). `None` for hardware or when there is no backend yet.
    #[serde(default)]
    pub backend_note: Option<String>,
}

/// Fallback reason for the UI badge. Deterministic triage from facts the
/// app owns (no probe access needed): test hook, platform, else the probe
/// itself failed (exact status lives in the session log). Pure + tested.
pub fn backend_note_for(backend: Option<&str>) -> Option<String> {
    match backend {
        None | Some("videotoolbox") | Some("nvenc") | Some("qsv") | Some("amf") | Some("mfhw") => {
            None
        }
        Some("openh264") => Some(if cfg!(target_os = "windows") {
            if std::env::var_os("GOLIVE_DISABLE_HW").is_some() {
                "hardware desabilitado (GOLIVE_DISABLE_HW)".to_owned()
            } else {
                "NVENC indisponível — usando OpenH264 (software)".to_owned()
            }
        } else if cfg!(not(target_os = "macos")) {
            "sem aceleração de hardware nesta plataforma".to_owned()
        } else if std::env::var_os("GOLIVE_DISABLE_HW").is_some() {
            "hardware desabilitado (GOLIVE_DISABLE_HW)".to_owned()
        } else {
            "probe de hardware falhou — ver log de sessão".to_owned()
        }),
        Some(_) => None,
    }
}

/// Effective share quality: the last profile accepted by `set_quality`
/// (or the `start_share` default) plus the encode generation fence.
/// Generation counts APPLIED reconfigs (rollbacks included) and arrives
/// async — see the `quality` media-event.
///
/// Note: odd requested dims are accepted and floored to even inside the
/// encoder (see `normalize_dims`) — the stored profile echoes the request.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct EffectiveQuality {
    pub profile: QualityProfile,
    pub generation: u64,
}
/// Roster member in the exact `RoomMember {id, nickname, master, share}`
/// shape the UI consumes. Same mapping `pump` uses for the roster
/// signal-event (`pump.rs` `Incoming::Roster`): fields copied verbatim from
/// the stored `signal.roster()` entries — no derivation from the owner
/// snapshot or links anywhere.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RosterMember {
    pub id: String,
    pub nickname: String,
    pub master: bool,
    pub share: bool,
}

/// Share-modal thumbnail (lazy one-shot pull, never polled). Always
/// succeeds at the command boundary: capture failures (denial, gone
/// source, encode) come back as a null `data_url` — the modal lists
/// sources regardless. Never carries titles or pixels except inside the
/// data URL itself.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SourcePreview {
    pub data_url: Option<String>,
    pub w: u32,
    pub h: u32,
}

/// Wire ids as decimal strings (adopted from envelopes, never logged).
#[derive(Clone, Default, Debug)]
pub struct WireIds {
    pub session: String,
    pub share: String,
    pub link: String,
    pub attempt: String,
}

/// One host link: its own PeerConnection + fences + trickle queue.
pub struct PublishSession {
    pub publisher: Arc<tokio::sync::Mutex<Publisher>>,
    pub owner_fence: Fence,
    pub wire: WireIds,
    pub remote_ready: bool,
    pub pending_remote: Vec<String>,
    /// Sole capture bridge for this shared encoder (Display/Window only).
    /// One live session holds it; unwatch transfers ownership to a survivor.
    /// The last session stops it; a later watch starts a fresh source.
    pub bridge: Option<screen::BridgeHandle>,
}

pub(crate) struct WatchSession {
    /// Independent output stream; the OS mixes simultaneous watched hosts.
    playback: Option<audio::ViewerPlayback>,
    pub fence: Fence,
    pub viewer: Option<Arc<tokio::sync::Mutex<NativeViewer>>>,
    pub adopted: Option<WireIds>,
    pub alive: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub remote_ready: bool,
    pub pending_remote: Vec<String>,
}

/// True when a watch session is already being torn down (window-close
/// auto-unwatch flipped liveness; teardown queued or done). Fresh intents
/// (`alive: None`, pre-adoption) and live sessions are NOT dying: only an
/// explicit `Some(false)` counts, so a re-watch replaces exactly the
/// sessions the death path abandoned — never a live viewer. Pure.
pub(crate) fn watch_session_is_dying(session: &WatchSession) -> bool {
    session
        .alive
        .as_ref()
        .is_some_and(|flag| !flag.load(std::sync::atomic::Ordering::Acquire))
}

/// Shared shell state. Managed as `Arc<AppState>` so background tasks and
/// tests can hold it without a Tauri app.
pub struct AppState {
    inner: Mutex<Inner>,
    /// Serialize command/signal operations across awaits; media callbacks and
    /// snapshots still use only the short Inner lock, so reconfig Stats can land.
    operations: tokio::sync::Mutex<()>,
    /// Session file log (packaged verification). Disabled until `run_with`
    /// inits it from the platform log dir; silent no-op before that (and
    /// in every unit test).
    session_log: Mutex<session_log::SessionLog>,
}

/// Self-driving test plan. ONLY constructible from the `--e2e-plan` CLI
/// flag: without it the app has zero test behavior (normal UI path).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct E2ePlan {
    /// "host" or "viewer".
    pub role: String,
    /// Rendezvous base, e.g. https://together.jouymaker.com.
    pub server: String,
    /// Room password for the run (local test only).
    pub password: String,
    /// Display name for the run.
    pub nickname: String,
    /// File where the host publishes the room code.
    pub code_file: String,
    /// File where this instance reports JSON status.
    pub status_file: String,
    /// Share source selector ("synthetic" default | "movie:<path>" |
    /// "display:<id>" | "window:<id>" | "camera:<id>" |
    /// "combo:display:<id>+camera:<cid>"). Optional so existing plans keep
    /// working unchanged (absent == synthetic). The viewer ignores it.
    #[serde(default)]
    pub share: Option<String>,
    /// Optional quality for the mid-share E2E check; absent preserves 360p15.
    #[serde(default)]
    pub quality: Option<QualityProfile>,
}

impl E2ePlan {
    /// Parses `--e2e-plan '<json>'` from process args. Returns `None` when
    /// the flag is absent (normal app). Errors on malformed plan/role.
    pub fn from_args(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let args: Vec<String> = args.collect();
        let Some(pos) = args.iter().position(|a| a == "--e2e-plan") else {
            return Ok(None);
        };
        let raw = args
            .get(pos + 1)
            .ok_or_else(|| "--e2e-plan needs a JSON argument".to_string())?;
        let plan: E2ePlan =
            serde_json::from_str(raw).map_err(|e| format!("bad --e2e-plan JSON: {e}"))?;
        if plan.role != "host" && plan.role != "viewer" {
            return Err("e2e plan role must be 'host' or 'viewer'".to_string());
        }
        for (name, value) in [
            ("server", &plan.server),
            ("password", &plan.password),
            ("nickname", &plan.nickname),
            ("code_file", &plan.code_file),
            ("status_file", &plan.status_file),
        ] {
            if value.trim().is_empty() {
                return Err(format!("e2e plan field '{name}' must not be empty"));
            }
        }
        if let Some(share) = plan.share.as_deref() {
            if share.trim().is_empty() {
                return Err("e2e plan field 'share' must not be empty".to_string());
            }
            // Early rejection: the host passes this straight to start_share,
            // so a typo here must fail at plan parse, not mid-run.
            ShareSource::parse(share).map_err(|e| format!("e2e plan field 'share': {e}"))?;
        }
        if let Some(quality) = plan.quality {
            quality
                .validate()
                .map_err(|e| format!("e2e quality: {e}"))?;
        }
        Ok(Some(plan))
    }
}

struct Inner {
    desktop: Option<AppHandle>,
    players: HashMap<String, player::Surface>,
    player_mute_all: bool,
    server_base: String,
    owner: Owner,
    signal: Option<SignalClient>,
    publishers: HashMap<String, PublishSession>,
    viewers: HashMap<String, WatchSession>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Present only when launched with `--e2e-plan`. Gates every test-only
    /// command; the normal UI path never sees it.
    e2e_plan: Option<E2ePlan>,
    /// Stored share source descriptor (set at start_share, cleared on
    /// stop/leave). Lets late watches (re-watch after unwatch, a second
    /// concurrent peer) build a FRESH publisher + bridge instead of reusing
    /// the single-shot idle template. Kind + id/path only — never titles,
    /// pixels, or tokens.
    share_source: Option<ShareSource>,
    /// Backend-observed media counters (forward tasks bump these).
    media_counters: MediaCounters,
    /// One native video window (+ its latest-only feed) per watched member.
    /// N links mean N independent windows; a dead helper fails one link.
    video_windows: HashMap<String, video::VideoWindow>,
    video_feeds: HashMap<String, video::FramePush>,
    /// Per-link present-window sequence, assigned at every spawn and logged
    /// on respawn so interleaved links stay distinguishable from one
    /// flapping link. Numeric only — never names, titles, or nicknames.
    video_seq: HashMap<String, u64>,
    /// Next present-window sequence number (starts at 1; 0 never appears).
    next_video_seq: u64,
    /// Per-link decode observations (bumped in the `on_frame` callback:
    /// decoded count + latest dims). Joined with the windows above into
    /// [`video::LinkStats`] on snapshot/emit — no polling anywhere.
    link_tracks: HashMap<String, LinkTrack>,
    /// Effective share quality (last accepted `set_quality` profile, or the
    /// `start_share` default). `None` when not sharing; generation bumps
    /// async as the encode thread applies reconfigs (see `media-event`
    /// `quality`). Cleared on stop/leave with the publishers.
    share_profile: Option<EffectiveQuality>,
    /// Live capture profile shared with the screen bridge (fps clamp +
    /// target dims, re-clamped by `set_quality` without restarting the OS
    /// stream). `None` for synthetic/movie (no bridge) and when idle.
    share_capture: Option<Arc<Mutex<QualityProfile>>>,
    /// System-audio tap + Opus fanout for the live share (Display/Window).
    audio: Option<audio::ShareAudio>,
    /// Session-log dedupe: last backend name logged + whether the ICE
    /// census line went out (log once per process, not per Stats event).
    last_logged_backend: Option<String>,
    census_logged: bool,
    /// Live modal previews by token (see `preview_start`). Own OS reads,
    /// independent from share bridges; stopped explicitly (modal close,
    /// blur, share confirm) and swept on stop_share/leave. Kind + id only.
    previews: PreviewRegistry,
    /// Stage self-view sessions by token (see `selfview_start`). Each owns
    /// its tap-forwarder stop flag; stopped explicitly (tile hide, share
    /// stop) and swept on stop_share/leave.
    selfviews: HashMap<String, SelfviewSession>,
}

/// One stage self-view session: owns the tap-forwarder stop flag so
/// `selfview_stop` joins promptly instead of stranding a thread in `recv`.
struct SelfviewSession {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct PendingPreview {
    cancel: Arc<AtomicBool>,
}

struct ActivePreview {
    cancel: Arc<AtomicBool>,
    handle: screen::PreviewHandle,
}

#[derive(Default)]
struct PreviewRegistry {
    pending: HashMap<String, PendingPreview>,
    active: HashMap<String, ActivePreview>,
}

impl PreviewRegistry {
    fn register(&mut self, token: String, cancel: Arc<AtomicBool>) -> bool {
        if self.pending.contains_key(&token) || self.active.contains_key(&token) {
            return false;
        }
        self.pending.insert(token, PendingPreview { cancel });
        true
    }

    fn promote(
        &mut self,
        token: &str,
        cancel: &Arc<AtomicBool>,
        handle: screen::PreviewHandle,
    ) -> Result<(), screen::PreviewHandle> {
        let is_current = self
            .pending
            .get(token)
            .map(|pending| Arc::ptr_eq(&pending.cancel, cancel))
            .unwrap_or(false);
        if !is_current || cancel.load(Ordering::Acquire) {
            return Err(handle);
        }
        self.pending.remove(token);
        self.active.insert(
            token.to_owned(),
            ActivePreview {
                cancel: Arc::clone(cancel),
                handle,
            },
        );
        Ok(())
    }

    fn remove_pending(&mut self, token: &str, cancel: &Arc<AtomicBool>) {
        if self
            .pending
            .get(token)
            .map(|pending| Arc::ptr_eq(&pending.cancel, cancel))
            .unwrap_or(false)
        {
            self.pending.remove(token);
        }
    }

    fn is_pending(&self, token: &str, cancel: &Arc<AtomicBool>) -> bool {
        !cancel.load(Ordering::Acquire)
            && self
                .pending
                .get(token)
                .map(|pending| Arc::ptr_eq(&pending.cancel, cancel))
                .unwrap_or(false)
    }

    fn take(&mut self, token: &str) -> Option<screen::PreviewHandle> {
        if let Some(pending) = self.pending.remove(token) {
            pending.cancel.store(true, Ordering::Release);
        }
        self.active.remove(token).map(|entry| {
            entry.cancel.store(true, Ordering::Release);
            entry.handle
        })
    }

    fn is_active(&self, token: &str, cancel: &Arc<AtomicBool>) -> bool {
        self.active
            .get(token)
            .map(|active| Arc::ptr_eq(&active.cancel, cancel) && !cancel.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    fn sweep(&mut self) -> Vec<screen::PreviewHandle> {
        for (_, pending) in self.pending.drain() {
            pending.cancel.store(true, Ordering::Release);
        }
        self.active
            .drain()
            .map(|(_, active)| {
                active.cancel.store(true, Ordering::Release);
                active.handle
            })
            .collect()
    }
}

impl SelfviewSession {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Decode-side observation for one watched member.
#[derive(Clone, Debug, Default)]
pub struct LinkTrack {
    pub title: String,
    pub decoded: u64,
    pub w: u32,
    pub h: u32,
}

/// Replacement gap: how long a self-view forwarder waits for the NEW bridge
/// after its feed disconnects (stop-first restarts, rewatch rebuilds) before
/// concluding the share ended. Stop-responsive slices; the sweep ends it
/// immediately on share stop.
const SELFVIEW_REATTACH_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
/// Self-view feed poll budget: the tile idles here, stop-responsive.
const SELFVIEW_RECV_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// Packs one tap frame as GLP2/format-1 (contiguous I420, player-compatible).
/// `None` on odd dims or bad length — the forwarder skips the frame instead
/// of breaking the tile (the frontend rejects odd dims the same way).
fn pack_selfview_frame(frame: &golive_core::media::I420Frame, seq: u32) -> Option<Vec<u8>> {
    if frame.w < 2 || frame.h < 2 || frame.w % 2 != 0 || frame.h % 2 != 0 {
        return None;
    }
    if frame.data.len() != frame.w * frame.h * 3 / 2 {
        return None;
    }
    let mut bytes = Vec::with_capacity(20 + frame.data.len());
    bytes.extend_from_slice(b"GLP2");
    bytes.extend_from_slice(&seq.to_le_bytes());
    bytes.extend_from_slice(&(frame.w as u32).to_le_bytes());
    bytes.extend_from_slice(&(frame.h as u32).to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&frame.data);
    Some(bytes)
}

/// Stage self-view forwarder: drains the current tap feed, surviving bridge
/// replacement without frontend action. On feed disconnect it re-resolves the
/// live bridge's tap (`reattach`; `None` = no share) — immediately, then
/// across `grace` while the share restarts — and only ends when no bridge
/// comes back, `stop` fires, or `send` fails. Sequence numbers continue
/// across reattachment (one tile stream, never restarted).
fn forward_selfview(
    mut feed: std::sync::mpsc::Receiver<golive_core::media::I420Frame>,
    stop: &AtomicBool,
    grace: std::time::Duration,
    reattach: &mut dyn FnMut() -> Option<std::sync::mpsc::Receiver<golive_core::media::I420Frame>>,
    send: &mut dyn FnMut(Vec<u8>) -> bool,
) {
    let mut seq = 0u32;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match feed.recv_timeout(SELFVIEW_RECV_BUDGET) {
            Ok(frame) => {
                if let Some(bytes) = pack_selfview_frame(&frame, seq) {
                    seq = seq.wrapping_add(1);
                    if !send(bytes) {
                        return;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Bridge replaced (restart/rebuild) or share ended: give the
                // new bridge its grace, stop-responsive, then conclude.
                let deadline = std::time::Instant::now() + grace;
                let mut replacement = reattach();
                while replacement.is_none() {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    replacement = reattach();
                }
                feed = replacement.expect("reattach just resolved");
            }
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                desktop: None,
                players: HashMap::new(),
                player_mute_all: false,
                server_base: DEFAULT_SERVER.to_owned(),
                owner: Owner::new(),
                signal: None,
                publishers: HashMap::new(),
                viewers: HashMap::new(),
                tasks: Vec::new(),
                e2e_plan: None,
                media_counters: MediaCounters::default(),
                video_windows: HashMap::new(),
                video_feeds: HashMap::new(),
                video_seq: HashMap::new(),
                next_video_seq: 1,
                link_tracks: HashMap::new(),
                share_profile: None,
                share_capture: None,
                audio: None,
                last_logged_backend: None,
                census_logged: false,
                share_source: None,
                previews: PreviewRegistry::default(),
                selfviews: HashMap::new(),
            }),
            session_log: Mutex::new(session_log::SessionLog::disabled()),
            operations: tokio::sync::Mutex::new(()),
        }
    }

    /// Installs the session file log (called once from `run_with` setup).
    pub fn set_session_log(&self, log: session_log::SessionLog) {
        if let Ok(mut slot) = self.session_log.lock() {
            *slot = log;
        }
    }

    /// One redacted milestone line (no-op until `set_session_log`).
    pub fn session_log(&self, line: String) {
        if let Ok(guard) = self.session_log.lock() {
            guard.log(&line);
        }
    }

    /// Installs the self-driving plan (called once at startup from CLI args).
    pub fn set_e2e_plan(&self, plan: E2ePlan) -> Result<(), String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?
            .e2e_plan = Some(plan);
        Ok(())
    }

    fn require_e2e_plan(&self) -> Result<E2ePlan, String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?
            .e2e_plan
            .clone()
            .ok_or_else(|| "e2e not active".to_string())
    }

    /// Sets the rendezvous base URL (`http(s)://host:port`). Validated.
    pub fn set_server(&self, base: &str) -> Result<String, String> {
        let base = base.trim().trim_end_matches('/').to_owned();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err("server must start with http:// or https://".into());
        }
        if base.len() > 256 {
            return Err("server URL too long".into());
        }
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?
            .server_base = base.clone();
        Ok(base)
    }

    fn base(&self) -> Result<String, String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())
            .map(|inner| inner.server_base.clone())
    }

    /// Creates a room on the rendezvous and starts the signal pump.
    /// Returns the room code (not a secret).
    pub async fn create_room(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        nickname: &str,
        password: &str,
    ) -> Result<String, String> {
        let _operation = self.operations.lock().await;
        let base = self.base()?;
        let join = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.owner.begin_join().map_err(redact_owner)?
        };
        let mut signal = match SignalClient::create_room(&base, nickname, password, false) {
            Ok(signal) => signal,
            Err(e) => {
                self.abort_join_fence(&join);
                return Err(format!("create room: {e}"));
            }
        };
        let code = signal.code().to_owned();
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if let Err(e) = inner.owner.complete_opened(&join).map_err(redact_owner) {
                drop(inner);
                signal.leave();
                signal.shutdown();
                self.abort_join_fence(&join);
                return Err(e);
            }
            inner.signal = Some(signal);
            // Test-only handoff: publish the room code where the plan says,
            // so a second self-driven instance can join without humans.
            if let Some(plan) = inner.e2e_plan.clone() {
                if let Some(parent) = std::path::Path::new(&plan.code_file).parent() {
                    if !parent.as_os_str().is_empty() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                }
                let _ = std::fs::write(&plan.code_file, &code);
            }
            inner.tasks.push(pump::spawn_pump(Arc::clone(self), app));
        }
        Ok(code)
    }

    /// Joins a room by code. Returns our member id (not a secret).
    pub async fn join_room(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        code: &str,
        nickname: &str,
        password: &str,
    ) -> Result<String, String> {
        let _operation = self.operations.lock().await;
        let base = self.base()?;
        let join = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.owner.begin_join().map_err(redact_owner)?
        };
        let mut signal = match SignalClient::join_room(&base, code, nickname, password) {
            Ok(signal) => signal,
            Err(e) => {
                self.abort_join_fence(&join);
                return Err(format!("join room: {e}"));
            }
        };
        let member_id = signal.member_id().to_owned();
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if let Err(e) = inner.owner.complete_opened(&join).map_err(redact_owner) {
                drop(inner);
                signal.leave();
                signal.shutdown();
                self.abort_join_fence(&join);
                return Err(e);
            }
            inner.signal = Some(signal);
            inner.tasks.push(pump::spawn_pump(Arc::clone(self), app));
        }
        Ok(member_id)
    }

    fn abort_join_fence(&self, fence: &golive_core::owner::Fence) {
        if let Ok(inner) = self.inner.lock() {
            let _ = inner.owner.abort_join(fence);
        }
    }

    /// Leaves the room: tasks aborted, media stopped, session closed
    /// best-effort. Idempotent.
    pub async fn leave(self: &Arc<Self>) -> Result<(), String> {
        let _operation = self.operations.lock().await;
        self.stop_all_previews();
        self.stop_all_selfviews();
        let (signal, publishers, viewers, audio) = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            for task in inner.tasks.drain(..) {
                task.abort();
            }
            inner.share_profile = None;
            inner.share_capture = None;
            inner.share_source = None;
            for session in inner.viewers.values() {
                if let Some(alive) = session.alive.as_ref() {
                    alive.store(false, std::sync::atomic::Ordering::Release);
                }
            }
            let audio = inner.audio.take();
            (
                inner.signal.take(),
                std::mem::take(&mut inner.publishers),
                std::mem::take(&mut inner.viewers),
                audio,
            )
        };
        drop(audio);
        if let Some(mut signal) = signal {
            signal.leave();
            signal.shutdown();
        }
        for (_, mut session) in publishers {
            session.publisher.lock().await.stop().await;
            if let Some(bridge) = session.bridge.as_mut() {
                bridge.stop();
            }
        }
        for (_, session) in viewers {
            if let Some(viewer) = session.viewer {
                viewer.lock().await.stop().await;
            }
        }
        self.close_all_video_windows();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?;
        // Best-effort close; absence of a session is not an error here.
        if let Ok(close) = inner.owner.begin_close() {
            let _ = inner.owner.complete_closed(&close);
        }
        drop(inner);
        self.session_log("leave".to_string());
        Ok(())
    }

    /// Starts sharing. One encode pipeline per watcher is created lazily on
    /// watch intents (fanout-via-shared-track is the next core refactor).
    pub async fn start_share(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        source: &str,
        profile: Option<QualityProfile>,
    ) -> Result<(), String> {
        self.start_share_with_cancel(app, source, profile, Arc::new(AtomicBool::new(false)))
            .await
    }

    async fn start_share_with_cancel(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        source: &str,
        profile: Option<QualityProfile>,
        startup_cancel: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let _operation = self.operations.lock().await;
        let source = ShareSource::parse(source)?;
        let start_profile = initial_share_profile(profile)?;
        let live_profile = Arc::new(Mutex::new(start_profile));
        let mut share_audio = match window_audio_id(&source) {
            Some(id) => audio::ShareAudio::start_for_window(id).ok(),
            None if matches!(source, ShareSource::Display(_)) => {
                // Display share starts with the default audio exclusions
                // (Discord + our own app/helper); the host can untoggle any
                // of them via set_audio_exclusions.
                audio::ShareAudio::start(golive_platform::default_excluded_tokens()).ok()
            }
            None if matches!(
                &source,
                ShareSource::Combo { screen, .. }
                    if matches!(screen.as_ref(), ShareSource::Display(_))
            ) =>
            {
                // Combo over a display: same default exclusions as a plain
                // display share (the webcam has no system-audio tap).
                audio::ShareAudio::start(golive_platform::default_excluded_tokens()).ok()
            }
            None => None,
        };
        let audio_rx = share_audio.as_ref().map(|session| session.subscribe());
        // Build the template session BEFORE touching lifecycle (pre-flight):
        // bridge setup may prompt/fail, and a failure must leave no
        // half-started share behind (Starting has no path back to Stopped).
        let (mut template, mut template_bridge, event_rx) = match Self::build_source_session(
            &source,
            start_profile,
            &live_profile,
            audio_rx,
            Arc::clone(&startup_cancel),
        )
        .await
        {
            Ok(built) => built,
            Err(e) => return Err(e),
        };
        // From here on, every failure path stops the template pieces.
        // Gate under one short lock (no await inside — a std guard must
        // never cross one); refusals stop the built pieces outside it.
        let start = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if inner.signal.is_none() {
                Err("not in a room".to_owned())
            } else {
                match inner.owner.begin_share_start() {
                    Ok(start) => Ok(start),
                    Err(e) => Err(redact_owner(e)),
                }
            }
        };
        let start = match start {
            Ok(start) => start,
            Err(e) => {
                template.stop().await;
                if let Some(bridge) = template_bridge.as_mut() {
                    bridge.stop();
                }
                return Err(e);
            }
        };
        let template = Arc::new(tokio::sync::Mutex::new(template));
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner
                .owner
                .complete_share_live(&start)
                .map_err(redact_owner)?;
            if let Some(signal) = inner.signal.as_ref() {
                let _ = signal.announce_share(true);
            }
            inner.tasks.push(pump::spawn_forward(
                Arc::clone(self),
                app,
                event_rx,
                pump::ForwardTarget::Share,
                Some(Arc::downgrade(&template)),
                None,
                None,
            ));
            // Stash the idle publisher as the template for the first watch.
            // Single-shot: adopting moves it to the watcher; later watches
            // rebuild fresh from `share_source` (see pump `on_watch`).
            let has_bridge = template_bridge.is_some();
            inner.publishers.insert(
                String::new(),
                PublishSession {
                    publisher: template,
                    owner_fence: start,
                    wire: WireIds::default(),
                    remote_ready: false,
                    pending_remote: Vec::new(),
                    bridge: template_bridge,
                },
            );
            inner.share_profile = Some(EffectiveQuality {
                profile: start_profile,
                generation: 0,
            });
            // Remember the source descriptor + shared live profile so late
            // watches can rebuild (re-watch after unwatch, second peer).
            inner.share_source = Some(source.clone());
            inner.share_capture = has_bridge.then(|| Arc::clone(&live_profile));
            inner.audio = share_audio.take();
        }
        // Milestone: source KIND only (never paths/ids) + start profile.
        let kind = match &source {
            ShareSource::Synthetic => "synthetic",
            ShareSource::Movie(_) => "movie",
            ShareSource::Display(_) => "display",
            ShareSource::Window(_) => "window",
            ShareSource::Camera(_) => "camera",
            ShareSource::Combo { .. } => "combo",
        };
        let audio_live = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner
                .audio
                .as_ref()
                .map(|session| session.live())
                .unwrap_or(false)
        };
        self.session_log(format!(
            "share start kind={kind} profile={}x{}@{} audio={}",
            start_profile.w, start_profile.h, start_profile.fps, audio_live as u8
        ));
        Ok(())
    }

    /// Builds one publisher session from a share source at `profile`.
    /// Display/Window open a fresh OS stream bridged into the publisher's
    /// External feed (sharing `live` for capture clamping); Synthetic/Movie
    /// resolve directly (Movie pre-flights the path, as before). Pre-flight
    /// safe: on `Err` nothing was started and nothing leaks. Shared by
    /// start_share's template and late watches (re-watch, second peer).
    async fn build_source_session(
        source: &ShareSource,
        profile: QualityProfile,
        live: &Arc<Mutex<QualityProfile>>,
        audio_rx: Option<std::sync::mpsc::Receiver<golive_platform::EncodedAudioPacket>>,
        startup_cancel: Arc<AtomicBool>,
    ) -> Result<
        (
            Publisher,
            Option<screen::BridgeHandle>,
            mpsc::UnboundedReceiver<MediaEvent>,
        ),
        String,
    > {
        enum Resolved {
            Direct(VideoSource),
            Bridged {
                video: VideoSource,
                bridge: screen::BridgeHandle,
            },
        }
        let resolved = match source {
            ShareSource::Synthetic => Resolved::Direct(VideoSource::SyntheticBall),
            ShareSource::Movie(path) => {
                if !std::path::Path::new(path).exists() {
                    return Err("movie file not found".into());
                }
                Resolved::Direct(VideoSource::MovieFile(path.into()))
            }
            ShareSource::Display(id) => {
                let (rx, bridge, label) = start_capture_for_blocking(
                    golive_platform::SourceKind::Display,
                    id.clone(),
                    profile,
                    Arc::clone(live),
                    Arc::clone(&startup_cancel),
                )
                .await
                .map_err(|e| e.to_string())?;
                Resolved::Bridged {
                    video: VideoSource::External(ExternalSource { rx, label }),
                    bridge,
                }
            }
            ShareSource::Window(id) => {
                let (rx, bridge, label) = start_capture_for_blocking(
                    golive_platform::SourceKind::Window,
                    id.clone(),
                    profile,
                    Arc::clone(live),
                    Arc::clone(&startup_cancel),
                )
                .await
                .map_err(|e| e.to_string())?;
                Resolved::Bridged {
                    video: VideoSource::External(ExternalSource { rx, label }),
                    bridge,
                }
            }
            ShareSource::Camera(id) => {
                let (rx, bridge, label) = start_capture_for_blocking(
                    golive_platform::SourceKind::Camera,
                    id.clone(),
                    profile,
                    Arc::clone(live),
                    Arc::clone(&startup_cancel),
                )
                .await
                .map_err(|e| e.to_string())?;
                Resolved::Bridged {
                    video: VideoSource::External(ExternalSource { rx, label }),
                    bridge,
                }
            }
            ShareSource::Combo { screen, camera } => {
                let (screen_kind, screen_id) = match screen.as_ref() {
                    ShareSource::Display(id) => (golive_platform::SourceKind::Display, id),
                    ShareSource::Window(id) => (golive_platform::SourceKind::Window, id),
                    _ => return Err("combo: a tela é display ou window".into()),
                };
                let (rx, bridge, label) = start_capture_combo_blocking(
                    screen_kind,
                    screen_id.clone(),
                    camera.clone(),
                    profile,
                    Arc::clone(live),
                    Arc::clone(&startup_cancel),
                )
                .await
                .map_err(|e| e.to_string())?;
                Resolved::Bridged {
                    video: VideoSource::External(ExternalSource { rx, label }),
                    bridge,
                }
            }
        };
        let (video_source, mut bridge) = match resolved {
            Resolved::Direct(video) => (video, None),
            Resolved::Bridged { video, bridge } => (video, Some(bridge)),
        };
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        // `Publisher::start` pins Quality::P720; late watches rebuild at the
        // CURRENT effective profile, so both go through `start_with_profile`
        // (Auto engine == `start`'s engine — identical behavior at P720).
        match Publisher::start_with_profile_and_audio(
            video_source,
            profile,
            EngineKind::Auto,
            None,
            event_tx,
            audio_rx,
        )
        .await
        {
            Ok(publisher) => Ok((publisher, bridge, event_rx)),
            Err(e) => {
                if let Some(bridge) = bridge.as_mut() {
                    bridge.stop();
                }
                Err(format!("publisher: {e}"))
            }
        }
    }

    /// Adopts an independent peer connection for a late watcher. Concurrent
    /// watchers share the existing capture and encoder; after the last one
    /// leaves, a re-watch opens a fresh source at the current profile. Also
    /// spawns the session's Share forward task. Returns false when sharing
    /// is no longer live or construction fails; the wire protocol is unchanged.
    pub(crate) async fn adopt_fresh_session(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        watcher: &str,
        fence: Fence,
        wire: WireIds,
    ) -> bool {
        // Caller on_watch holds operations across this build/adoption.
        // Gate + snapshot under one short lock; the build runs outside it.
        let (source, live, profile, audio_rx) = {
            let inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(_) => return false,
            };
            if inner.owner.snapshot().share.state != golive_core::state::ShareState::Live
                || !inner.owner.link_is_current(&fence)
            {
                return false;
            }
            let Some(source) = inner.share_source.clone() else {
                return false;
            };
            let profile = inner
                .share_profile
                .map(|s| s.profile)
                .unwrap_or_else(|| Quality::P720.profile());
            // Display/Window/Camera/Combo share the stored live Arc; its
            // absence alongside a capture source is inconsistent — refuse
            // rather than fork it.
            let live = match &source {
                ShareSource::Display(_)
                | ShareSource::Window(_)
                | ShareSource::Camera(_)
                | ShareSource::Combo { .. } => match inner.share_capture.clone() {
                    Some(live) => live,
                    None => return false,
                },
                ShareSource::Synthetic | ShareSource::Movie(_) => Arc::new(Mutex::new(profile)),
            };
            let audio_rx = inner.audio.as_ref().map(|session| session.subscribe());
            (source, live, profile, audio_rx)
        };
        // Existing viewers already own a capture/encoder for this share.
        // Fork only transport; the single bridge remains with one live session.
        let existing = self.inner.lock().ok().and_then(|inner| {
            inner
                .publishers
                .values()
                .next()
                .map(|s| s.publisher.clone())
        });
        let (publisher, mut bridge, event_rx) = if let Some(existing) = existing {
            let (tx, rx) = mpsc::unbounded_channel();
            match existing.lock().await.fork(None, tx, audio_rx).await {
                Ok(publisher) => (publisher, None, rx),
                Err(_) => return false,
            }
        } else {
            match Self::build_source_session(
                &source,
                profile,
                &live,
                audio_rx,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            {
                Ok(built) => built,
                Err(_) => return false,
            }
        };
        let publisher = Arc::new(tokio::sync::Mutex::new(publisher));
        // Decide + insert under one short lock with NO await inside (a std
        // guard must never cross an await — it would poison Send for every
        // caller: pump spawn, Tauri commands). Teardown runs outside.
        enum FreshDecision {
            Inserted,
            ShareGone,
            Taken,
        }
        let decision = match self.inner.lock().ok() {
            // `ok()` drops a PoisonError (which owns the guard) at once, so
            // only a live guard enters the Some arm — and no await runs
            // inside either arm.
            Some(mut inner) => {
                // The share may have died while the build ran: refuse instead
                // of resurrecting anything.
                if inner.owner.snapshot().share.state != golive_core::state::ShareState::Live
                    || !inner.owner.link_is_current(&fence)
                {
                    FreshDecision::ShareGone
                // A concurrent watch may have adopted first: keep the winner
                // and drop the spare (a session IS live, so the caller still
                // proceeds to offer from it).
                } else if inner.publishers.contains_key(watcher) {
                    FreshDecision::Taken
                } else {
                    inner.publishers.insert(
                        watcher.to_owned(),
                        PublishSession {
                            publisher: Arc::clone(&publisher),
                            owner_fence: fence,
                            wire,
                            remote_ready: false,
                            pending_remote: Vec::new(),
                            // `take` keeps `bridge` initialized on every path
                            // (None after a successful insert) so the teardown
                            // below stays well-formed.
                            bridge: bridge.take(),
                        },
                    );
                    inner.tasks.push(pump::spawn_forward(
                        Arc::clone(self),
                        app,
                        event_rx,
                        pump::ForwardTarget::Share,
                        Some(Arc::downgrade(&publisher)),
                        None,
                        None,
                    ));
                    FreshDecision::Inserted
                }
            }
            // Poisoned lock: no guard was ever acquired; the spare dies
            // outside the lock below.
            None => FreshDecision::ShareGone,
        };
        match decision {
            FreshDecision::Inserted => {}
            // Spare / stillborn build: stop it outside the lock.
            FreshDecision::ShareGone => {
                stop_fresh_parts(&publisher, &mut bridge).await;
                return false;
            }
            FreshDecision::Taken => {
                stop_fresh_parts(&publisher, &mut bridge).await;
            }
        }
        // Milestone: source KIND only (never watcher ids, paths, or tokens).
        let kind = match &source {
            ShareSource::Synthetic => "synthetic",
            ShareSource::Movie(_) => "movie",
            ShareSource::Display(_) => "display",
            ShareSource::Window(_) => "window",
            ShareSource::Camera(_) => "camera",
            ShareSource::Combo { .. } => "combo",
        };
        self.session_log(format!("watch fresh kind={kind}"));
        true
    }

    /// Stops sharing: media stopped, bridges torn down, links cleared
    /// deterministically.
    pub async fn stop_share(self: &Arc<Self>) -> Result<(), String> {
        let _operation = self.operations.lock().await;
        self.stop_all_previews();
        self.stop_all_selfviews();
        let (publishers, audio) = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.share_profile = None;
            inner.share_capture = None;
            inner.share_source = None;
            let publishers = std::mem::take(&mut inner.publishers);
            let audio = inner.audio.take();
            (publishers, audio)
        };
        drop(audio);
        for (_, mut session) in publishers {
            session.publisher.lock().await.stop().await;
            if let Some(bridge) = session.bridge.as_mut() {
                bridge.stop();
            }
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?;
        // Idempotent: completing an already-stopped share is a no-op.
        if inner.owner.snapshot().share.id.is_some() {
            if let Ok(stop) = inner.owner.begin_share_stop() {
                let _ = inner.owner.complete_share_stopped(&stop);
            }
        } else {
            let _ = inner.owner.complete_share_stopped(&Fence::idle());
        }
        if let Some(signal) = inner.signal.as_ref() {
            let _ = signal.announce_share(false);
        }
        drop(inner);
        self.session_log("share stop".to_string());
        Ok(())
    }

    /// Live quality switch without re-signaling.
    ///
    /// Why a separate command (and `start_share` keeps its `{source}`-only
    /// wire shape): starting owns irreversible lifecycle pre-flight (owner
    /// fence Starting→Live has no path back, bridge acquisition, atomic
    /// teardown on failure). Quality is a property of a LIVE share; mixing
    /// profile validation into start would entangle it with those teardown
    /// paths. `set_quality` covers both the pre-watch template publisher
    /// and adopted per-watcher links.
    ///
    /// Semantics: validate-first via [`QualityProfile::validate`] (typed,
    /// redacted — numbers only), then transactional reconfig on every live
    /// publisher (core rebuilds + forces IDR + bumps the generation fence;
    /// same m-line, no re-signaling). Core apply is atomic per encoder
    /// (build-new-then-swap: a failed rebuild keeps the old encoder
    /// running). On partial multi-publisher failure this rolls back the
    /// ones that succeeded (best-effort, each rollback is itself a fenced
    /// reconfig) and leaves `share_profile` untouched — the encoder is
    /// never left in an intermediate state.
    ///
    /// Returns the stored effective profile (never older than the observed
    /// generation); the authoritative generation arrives async via
    /// `media-event` `quality` (emitted here optimistically and again by
    /// the forward task when it observes the fence bump) and via
    /// `get_media_counters.effective`.
    pub async fn set_quality(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        args: SetQualityArgs,
    ) -> Result<EffectiveQuality, String> {
        let _operation = self.operations.lock().await;
        if let Some(preset) = args.preset.as_deref() {
            match preset {
                "low" | "medium" | "high" => {}
                _ => return Err(format!("unknown preset '{preset}' (low|medium|high)")),
            }
        }
        let profile = QualityProfile {
            w: args.w,
            h: args.h,
            bitrate_kbps: args.bitrate_kbps,
            fps: args.fps,
        };
        profile.validate().map_err(|e| format!("qualidade: {e}"))?;
        // Snapshot publishers + previous effective under one short lock;
        // no lock is held across the awaits below.
        let (publishers, previous) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if inner.publishers.is_empty() {
                return Err("not sharing".into());
            }
            let publishers: Vec<Arc<tokio::sync::Mutex<Publisher>>> = inner
                .publishers
                .values()
                .map(|session| Arc::clone(&session.publisher))
                .collect();
            let previous = inner.share_profile.unwrap_or(EffectiveQuality {
                profile: Quality::P720.profile(),
                generation: 0,
            });
            (publishers, previous)
        };
        // Apply to every live publisher (template + adopted links).
        let mut applied = 0usize;
        let mut apply_error: Option<String> = None;
        for publisher in &publishers {
            match publisher.lock().await.reconfigure(profile) {
                Ok(()) => applied += 1,
                Err(e) => {
                    apply_error = Some(format!("qualidade: {e}"));
                    break;
                }
            }
        }
        if let Some(error) = apply_error {
            // Best-effort rollback: restore the previous profile on the
            // publishers that already moved (each rollback is fenced too).
            for publisher in publishers.iter().take(applied) {
                let _ = publisher.lock().await.reconfigure(previous.profile);
            }
            return Err(error);
        }
        // Restart the capture streams at the new profile (if bridged).
        // One live session owns the shared bridge. Visiting all sessions
        // finds that owner even after the original watcher has left. Transactional per stream (the new
        // OS stream starts first; a failure leaves the old one running)
        // with best-effort rollback of the streams that already moved plus
        // the publishers that already applied. Handles leave Inner for the
        // blocking calls (no long-held lock) and are restored after, unless
        // the share died under us (stop instead of resurrecting) or a
        // session turned over mid-switch (stop the orphan).
        let mut bridges: Vec<(String, screen::BridgeHandle)> = match self.inner.lock() {
            Ok(mut inner) => inner
                .publishers
                .iter_mut()
                .filter_map(|(watcher, session)| {
                    session
                        .bridge
                        .take()
                        .map(|bridge| (watcher.clone(), bridge))
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        if !bridges.is_empty() {
            let mut moved = 0usize;
            let mut bridge_error: Option<String> = None;
            for (_, handle) in bridges.iter_mut() {
                match handle.reconfigure(profile) {
                    Ok(()) => moved += 1,
                    Err(e) => {
                        bridge_error = Some(format!("qualidade: captura: {e}"));
                        break;
                    }
                }
            }
            if let Some(error) = bridge_error {
                for publisher in publishers.iter().take(applied) {
                    let _ = publisher.lock().await.reconfigure(previous.profile);
                }
                for (_, handle) in bridges.iter_mut().take(moved) {
                    let _ = handle.reconfigure(previous.profile);
                }
                if let Ok(mut inner) = self.inner.lock() {
                    restore_bridges(&mut inner, bridges);
                } else {
                    // Wedged lock: stop everything taken rather than leak OS
                    // streams with no owner.
                    for (_, mut handle) in bridges {
                        let _ = handle.stop();
                    }
                }
                return Err(error);
            }
        }
        let mut effective = EffectiveQuality {
            profile,
            generation: previous.generation,
        };
        {
            if let Ok(mut inner) = self.inner.lock() {
                if inner.publishers.is_empty() {
                    // Share died mid-switch: stop the (reconfigured) bridges
                    // instead of resurrecting them, report cleanly.
                    for (_, mut handle) in bridges {
                        let _ = handle.stop();
                    }
                    return Err("not sharing".into());
                }
                // Never write the generation backwards: the encode loop may
                // have applied the reconfig (and the forward task the fence
                // bump) while the bridge reconfigure awaited above. Re-read
                // under this same lock and take the max.
                let observed = inner.share_profile.map(|s| s.generation).unwrap_or(0);
                effective.generation = effective.generation.max(observed);
                inner.share_profile = Some(effective);
                // Re-clamp capture in place (every live bridge reads it per tick).
                if let Some(live) = inner.share_capture.as_ref() {
                    if let Ok(mut guard) = live.lock() {
                        *guard = profile;
                    }
                }
                restore_bridges(&mut inner, bridges);
            }
        }
        if let Some(app) = app {
            let _ = app.emit(
                "media-event",
                &serde_json::json!({
                    "kind": "quality",
                    "profile": profile,
                    "generation": effective.generation,
                }),
            );
        }
        self.session_log(format!(
            "quality profile={}x{}@{} generation={}",
            profile.w, profile.h, profile.fps, effective.generation
        ));
        Ok(effective)
    }

    /// Registers watch intent for a member (viewer side).
    pub async fn watch(self: &Arc<Self>, member: &str) -> Result<Fence, String> {
        let _operation = self.operations.lock().await;
        if member.trim().is_empty() {
            return Err("member must not be empty".into());
        }
        let (fence, old) = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if inner.signal.is_none() {
                return Err("not in a room".into());
            }
            // Repeated UI/roster intent is idempotent. Replacing this entry
            // loses ownership of the live viewer and its callbacks.
            if let Some(session) = inner.viewers.get(member) {
                if !watch_session_is_dying(session) {
                    return Ok(session.fence);
                }
                // A window-close auto-unwatch is still tearing this session
                // down (or queued it): take the dying entry out for teardown
                // below and start a fresh intent, instead of adopting a
                // fence with no live viewer behind it. The queued teardown
                // stands down on the fresh entry (see push_present_frame).
                // Same attempt-advance semantics as a normal intent: the
                // owner link is advanced, never re-rolled.
                let old = inner.viewers.remove(member);
                let fence = inner.owner.watch_remote(member).map_err(redact_owner)?;
                (fence, old)
            } else {
                let fence = inner.owner.watch_remote(member).map_err(redact_owner)?;
                (fence, None)
            }
        };
        // Retire the dying session outside the lock: stop its viewer (core
        // stop is idempotent) and forget its window/feed/track entries so
        // no ghost pipeline survives. Playback dies with the session value.
        if let Some(old) = old {
            if let Some(viewer) = old.viewer {
                viewer.lock().await.stop().await;
            }
            self.close_video_window(member);
        }
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.viewers.insert(
                member.to_owned(),
                WatchSession {
                    playback: None,
                    fence,
                    viewer: None,
                    adopted: None,
                    alive: None,
                    remote_ready: false,
                    pending_remote: Vec::new(),
                },
            );
            if let Some(signal) = inner.signal.as_ref() {
                signal
                    .watch(member, true)
                    .map_err(|e| format!("watch: {e}"))?;
            } else {
                return Err("not in a room".into());
            }
        }
        self.session_log(format!("watch member={}", session_log::short_id(member)));
        Ok(fence)
    }

    /// Removes our watch intent and tears down viewer media.
    pub async fn unwatch(self: &Arc<Self>, member: &str) -> Result<(), String> {
        let _operation = self.operations.lock().await;
        let session = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if let Some(signal) = inner.signal.as_ref() {
                let _ = signal.watch(member, false);
            }
            let session = inner.viewers.remove(member);
            if let Some(alive) = session.as_ref().and_then(|item| item.alive.as_ref()) {
                alive.store(false, std::sync::atomic::Ordering::Release);
            }
            if inner.viewers.is_empty() {
                inner.media_counters.connected = false;
                inner.media_counters.frames = 0;
                inner.media_counters.keyframes = 0;
                inner.media_counters.keyframes_seen = false;
            }
            session
        };
        if let Some(viewer) = session.and_then(|item| item.viewer) {
            viewer.lock().await.stop().await;
        }
        self.close_video_window(member);
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?;
        if let Ok(fence) = inner.owner.unwatch_remote(member) {
            let _ = inner.owner.complete_link_removed(&fence);
        }
        drop(inner);
        self.session_log(format!("unwatch member={}", session_log::short_id(member)));
        Ok(())
    }

    /// Lists capture sources for the UI select. Empty/error surfaces typed
    /// (the UI shows the reason instead of failing mute). May trigger the
    /// OS permission prompt on first use — call from explicit user action.
    pub async fn list_sources(&self) -> Result<Vec<screen::ListedSource>, String> {
        // enumerate() can block on the OS (prompt wait); keep it off the
        // async worker via spawn_blocking.
        tokio::task::spawn_blocking(screen::enumerate_sources)
            .await
            .map_err(|e| format!("listagem: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Compile-time capture capabilities (UI disable-with-reason).
    /// No OS contact: safe to call any time, never prompts.
    pub fn source_capabilities(&self) -> screen::Capabilities {
        screen::capabilities()
    }

    pub async fn list_audio_apps(&self) -> Vec<AudioApp> {
        tokio::task::spawn_blocking(audio::list_apps)
            .await
            .unwrap_or_default()
    }

    pub async fn set_audio_exclusions(self: &Arc<Self>, apps: Vec<String>) -> Result<(), String> {
        let _operation = self.operations.lock().await;
        let mut audio = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.audio.take()
        };
        let Some(mut session) = audio.take() else {
            return Err("not sharing".into());
        };
        let result = session.set_exclusions(apps);
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.audio = Some(session);
        }
        result
    }

    /// Immutable snapshot for the UI. Locks only to clone.
    pub fn get_snapshot(&self) -> Result<OwnerSnapshot, String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())
            .map(|inner| inner.owner.snapshot())
    }

    /// Rich roster pull for the UI (explicit callers only — never polled).
    /// Reads the same stored `signal.roster()` the pump maps into the
    /// `roster` signal-event, so a UI that mounted after that emit (listeners
    /// attach on room entry; Tauri events have no backlog) still sees
    /// nicknames on entry. Empty when offline (no signal client yet) or when
    /// the lock is poisoned — the event listener fills it in later.
    pub fn get_roster(&self) -> Vec<RosterMember> {
        let inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return Vec::new(),
        };
        match inner.signal.as_ref() {
            Some(signal) => signal
                .roster()
                .entries
                .iter()
                .map(|e| RosterMember {
                    id: e.id.clone(),
                    nickname: e.nickname.clone(),
                    master: e.master,
                    share: e.share,
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// One-shot source thumbnail for the share modal (explicit UI pull,
    /// never polled). Infallible at the boundary: unknown kind and every
    /// backend failure (denial, gone source, empty grab, encode) come back
    /// as a null `data_url` with 0x0 dims — the modal lists sources
    /// regardless. Never logs titles or pixels.
    pub fn preview_source(&self, kind: &str, id: &str) -> SourcePreview {
        let kind = match kind {
            "display" => golive_platform::SourceKind::Display,
            "window" => golive_platform::SourceKind::Window,
            "camera" => golive_platform::SourceKind::Camera,
            _ => {
                return SourcePreview {
                    data_url: None,
                    w: 0,
                    h: 0,
                };
            }
        };
        match screen::preview_source(kind, id) {
            Ok(preview) => SourcePreview {
                data_url: Some(preview.data_url),
                w: preview.w,
                h: preview.h,
            },
            Err(_) => SourcePreview {
                data_url: None,
                w: 0,
                h: 0,
            },
        }
    }

    /// Starts a live modal preview for one listed source. Returns an opaque
    /// token; frames flow as GLP2/format-0 (player-compatible) on `channel`
    /// until `preview_stop`, modal close/blur (frontend), share confirm
    /// (which stops first, then shares), or stop_share/leave (sweep).
    /// Own OS reads, independent from share bridges — a busy device (e.g.
    /// already shared elsewhere) fails typed, never silently.
    pub fn preview_start(&self, kind: &str, id: &str, channel: Channel) -> Result<String, String> {
        self.preview_start_with_cancel(kind, id, channel, Arc::new(AtomicBool::new(false)))
    }

    fn preview_start_with_cancel(
        &self,
        kind: &str,
        id: &str,
        channel: Channel,
        cancel: Arc<AtomicBool>,
    ) -> Result<String, String> {
        let kind = match kind {
            "display" => golive_platform::SourceKind::Display,
            "window" => golive_platform::SourceKind::Window,
            "camera" => golive_platform::SourceKind::Camera,
            _ => return Err("preview: fonte é display, window ou camera".into()),
        };
        let token = {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            format!(
                "pv-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
        };
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if !inner.previews.register(token.clone(), Arc::clone(&cancel)) {
                return Err("preview: registro ocupado".into());
            }
        }
        let still_pending = self
            .inner
            .lock()
            .map(|inner| inner.previews.is_pending(&token, &cancel))
            .unwrap_or(false);
        if !still_pending {
            return Err("preview: inicialização cancelada".into());
        }
        let (rx, handle) =
            match screen::start_preview_stream_with_cancel(kind, id, Arc::clone(&cancel)) {
                Ok(started) => started,
                Err(error) => {
                    cancel.store(true, Ordering::Release);
                    if let Ok(mut inner) = self.inner.lock() {
                        inner.previews.remove_pending(&token, &cancel);
                    }
                    return Err(error.to_string());
                }
            };
        let promoted = match self.inner.lock() {
            Ok(mut inner) => inner.previews.promote(&token, &cancel, handle),
            Err(_) => Err(handle),
        };
        if let Err(mut stale_handle) = promoted {
            cancel.store(true, Ordering::Release);
            let _ = stale_handle.stop();
            return Err("preview: inicialização cancelada".into());
        }
        // Forwarder: packets → GLP2 → Channel; exits when the pump ends
        // (stop or device failure) or the webview stops receiving. The pump
        // owns the lifetime (its end drops the channel side); explicit stops
        // and the stop_share/leave sweep bound everything else.
        if std::thread::Builder::new()
            .name("golive-preview-send".into())
            .spawn(move || {
                for packet in rx {
                    if channel
                        .send(InvokeResponseBody::Raw(packet.glp2_bytes()))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .is_err()
        {
            self.preview_stop(&token);
            return Err("preview: sem thread de envio".into());
        }
        if !self
            .inner
            .lock()
            .map(|inner| inner.previews.is_active(&token, &cancel))
            .unwrap_or(false)
        {
            self.preview_stop(&token);
            return Err("preview: inicialização cancelada".into());
        }
        // Milestone: kind only (never ids).
        let kind_name = match kind {
            golive_platform::SourceKind::Display => "display",
            golive_platform::SourceKind::Window => "window",
            golive_platform::SourceKind::Camera => "camera",
        };
        self.session_log(format!("preview start kind={kind_name}"));
        Ok(token)
    }

    /// Stops one live preview. Idempotent; unknown tokens are Ok. Takes the
    /// handle out of the registry under one short lock and joins it (bounded
    /// ~2 s) AFTER the lock is released — never a join under the mutex.
    pub fn preview_stop(&self, token: &str) {
        let handle = self
            .inner
            .lock()
            .ok()
            .and_then(|mut inner| inner.previews.take(token));
        if let Some(mut handle) = handle {
            let _ = handle.stop();
        }
    }

    /// Stops every live preview (stop_share/leave sweep).
    pub fn stop_all_previews(&self) {
        let handles: Vec<screen::PreviewHandle> = match self.inner.lock() {
            Ok(mut inner) => inner.previews.sweep(),
            Err(_) => Vec::new(),
        };
        for mut handle in handles {
            let _ = handle.stop();
        }
    }

    /// Attaches to the live share bridge's self-view tap (`None` when
    /// nothing capturable is shared). One short lock; the self-view
    /// forwarder calls this on start and on every bridge replacement.
    fn current_tap(&self) -> Option<std::sync::mpsc::Receiver<golive_core::media::I420Frame>> {
        let inner = self.inner.lock().ok()?;
        inner
            .publishers
            .values()
            .find_map(|session| session.bridge.as_ref().map(|bridge| bridge.attach_tap()))
    }

    /// Starts the stage self-view: mirrors the LIVE share bridge feed into
    /// `channel` as GLP2/format-1 (contiguous I420, player-compatible) until
    /// `selfview_stop`, share stop, or leave. No second OS open — the local
    /// tile reuses the exact frames the encoder gets. Fails honestly when
    /// nothing is shared (no bridge to tap). Survives bridge replacement
    /// (quality restarts, rewatch rebuilds): the forwarder re-attaches to
    /// the new bridge's tap on its own — no frontend resubscribe, no param
    /// changes.
    pub fn selfview_start(self: &Arc<Self>, channel: Channel) -> Result<String, String> {
        let initial = self.current_tap().ok_or_else(|| {
            let sharing = self
                .inner
                .lock()
                .map(|inner| !inner.publishers.is_empty())
                .unwrap_or(false);
            // Sem bridge com share no ar = fonte sem captura (synthetic ou
            // movie): erro próprio, nunca "inicie o compartilhamento".
            if sharing {
                "prévia local só para tela, janela ou webcam".to_string()
            } else {
                "inicie o compartilhamento para ver seu vídeo".to_string()
            }
        })?;
        let token = {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            format!(
                "sv-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_ = Arc::clone(&stop);
        let state = Arc::clone(self);
        let thread = std::thread::Builder::new()
            .name("golive-selfview-send".into())
            .spawn(move || {
                let mut reattach = move || state.current_tap();
                let mut send =
                    move |bytes: Vec<u8>| channel.send(InvokeResponseBody::Raw(bytes)).is_ok();
                forward_selfview(
                    initial,
                    &stop_,
                    SELFVIEW_REATTACH_WAIT,
                    &mut reattach,
                    &mut send,
                );
            })
            .map_err(|_| "prévia local: sem thread de envio".to_string())?;
        self.session_log("selfview start".to_string());
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?
            .selfviews
            .insert(
                token.clone(),
                SelfviewSession {
                    stop,
                    thread: Some(thread),
                },
            );
        Ok(token)
    }

    /// Stops one stage self-view. Idempotent; unknown tokens are Ok.
    pub fn selfview_stop(&self, token: &str) {
        let session = match self.inner.lock() {
            Ok(mut inner) => inner.selfviews.remove(token),
            Err(_) => None,
        };
        if let Some(mut session) = session {
            session.stop();
        }
    }

    /// Stops every stage self-view (stop_share/leave sweep).
    pub fn stop_all_selfviews(&self) {
        let sessions: Vec<SelfviewSession> = match self.inner.lock() {
            Ok(mut inner) => inner.selfviews.drain().map(|(_, s)| s).collect(),
            Err(_) => Vec::new(),
        };
        for mut session in sessions {
            session.stop();
        }
    }

    /// Backend-observed media counters (observational, redacted). Pollable
    /// fallback next to push events. `presented` is summed live from the
    /// native windows (acks), the rest is bumped by forward tasks.
    pub fn get_media_counters(&self) -> Result<MediaCounters, String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())
            .map(|inner| {
                let mut counters = inner.media_counters.clone();
                counters.presented = inner
                    .video_windows
                    .values()
                    .map(|w| w.presented())
                    .sum::<u64>()
                    + inner.players.values().map(|p| p.presented).sum::<u64>();
                counters.links = Self::link_stats_locked(&inner);
                counters.effective = inner.share_profile;
                counters.backend = Self::encode_backend_locked(&inner);
                counters.backend_note = backend_note_for(counters.backend.as_deref());
                counters
            })
    }

    /// First live encoder backend across publishers (non-blocking read;
    /// `None` until a build lands). All publishers share the engine, so
    /// first-reporter is representative.
    fn encode_backend_locked(inner: &Inner) -> Option<String> {
        inner
            .publishers
            .values()
            .filter_map(|session| session.publisher.try_lock().ok())
            .filter_map(|publisher| publisher.backend())
            .map(|name| name.to_owned())
            .next()
    }

    /// Builds per-link stats from decode tracks + native windows. Sorted by
    /// member for stable snapshots.
    fn link_stats_locked(inner: &Inner) -> Vec<video::LinkStats> {
        let mut links: Vec<video::LinkStats> = inner
            .video_windows
            .iter()
            .map(|(member, window)| {
                let track = inner.link_tracks.get(member);
                let decoded = track.map(|t| t.decoded).unwrap_or_else(|| window.pushed());
                let presented = window.presented();
                let (w, h) = window.resolution();
                video::LinkStats {
                    member: member.clone(),
                    title: track
                        .map(|t| t.title.clone())
                        .unwrap_or_else(|| window.title().to_owned()),
                    codec: video::LINK_CODEC.to_owned(),
                    width: track.map(|t| t.w).filter(|w| *w > 0).unwrap_or(w),
                    height: track.map(|t| t.h).filter(|h| *h > 0).unwrap_or(h),
                    decoded,
                    presented,
                    dropped: decoded.saturating_sub(presented),
                    render_fps: window.render_fps(),
                    bitrate_bps: window.bitrate_bps(),
                    bitrate_note: video::BITRATE_NOTE.to_owned(),
                    delay_estimate_ms: None,
                    delay_note: video::DELAY_NOTE.to_owned(),
                    dropped_note: video::DROPPED_NOTE.to_owned(),
                }
            })
            .collect();
        links.extend(inner.players.iter().filter_map(|(member, surface)| {
            let track = inner.link_tracks.get(member)?;
            let mut stats = surface.stats.lock().ok()?;
            let now = std::time::Instant::now();
            Some(video::LinkStats {
                member: member.clone(),
                title: track.title.clone(),
                codec: video::LINK_CODEC.into(),
                width: track.w,
                height: track.h,
                decoded: track.decoded,
                presented: surface.presented,
                dropped: track.decoded.saturating_sub(surface.presented),
                render_fps: stats.render_fps(now),
                bitrate_bps: stats.bitrate_bps(now),
                bitrate_note: video::BITRATE_NOTE.into(),
                delay_estimate_ms: None,
                delay_note: video::DELAY_NOTE.into(),
                dropped_note: video::DROPPED_NOTE.into(),
            })
        }));
        links.sort_by(|a, b| a.member.cmp(&b.member));
        links
    }

    /// Per-link stats for one member (used by the stats event emit).
    pub fn link_stats_for(&self, member: &str) -> Option<video::LinkStats> {
        self.inner.lock().ok().and_then(|inner| {
            Self::link_stats_locked(&inner)
                .into_iter()
                .find(|l| l.member == member)
        })
    }

    /// Records one decoded frame for a watched member (called from the
    /// `on_frame` present callback: event-driven, never polled).
    pub fn note_link_frame(
        &self,
        member: &str,
        title: &str,
        w: u32,
        h: u32,
        alive: &std::sync::atomic::AtomicBool,
    ) {
        if let Ok(mut inner) = self.inner.lock() {
            if !alive.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let track = inner.link_tracks.entry(member.to_owned()).or_default();
            track.decoded += 1;
            if track.title.is_empty() {
                track.title = title.to_owned();
            }
            if w > 0 && h > 0 {
                track.w = w;
                track.h = h;
            }
        }
    }

    /// Tears down (and forgets) the video window + feed for one member.
    /// Idempotent; bounded (feeder joins promptly, child reaped).
    pub fn close_video_window(&self, member: &str) {
        self.remove_player(member);
        let mut window = {
            match self.inner.lock() {
                Ok(mut inner) => {
                    inner.video_feeds.remove(member);
                    inner.link_tracks.remove(member);
                    inner.video_seq.remove(member);
                    inner.video_windows.remove(member)
                }
                Err(_) => None,
            }
        };
        if let Some(window) = window.as_mut() {
            window.stop();
        }
    }

    /// Tears down all video windows. Idempotent; bounded.
    pub fn close_all_video_windows(&self) {
        let members = self
            .inner
            .lock()
            .map(|inner| inner.players.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for member in members {
            self.remove_player(&member);
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.player_mute_all = false;
        }
        let mut windows = match self.inner.lock() {
            Ok(mut inner) => {
                inner.video_feeds.clear();
                inner.link_tracks.clear();
                inner.video_seq.clear();
                std::mem::take(&mut inner.video_windows)
            }
            Err(_) => return,
        };
        for (_, mut window) in windows.drain() {
            window.stop();
        }
    }

    // -- test-only e2e surface (all gated on the CLI plan) ----------------

    /// Returns the active self-driving plan, or `None` in normal runs.
    /// The frontend boots the e2e driver only when this is `Some`.
    pub fn get_e2e_plan(&self) -> Option<E2ePlan> {
        self.inner.lock().ok()?.e2e_plan.clone()
    }

    /// Writes a JSON status payload to the plan's status file. Gated on the
    /// plan; payloads carrying secret-looking keys are refused so artifacts
    /// stay clean even if the driver has a bug.
    pub fn e2e_status(&self, payload: &str) -> Result<(), String> {
        let plan = self.require_e2e_plan()?;
        if payload.len() > 4096 {
            return Err("e2e status payload too large".into());
        }
        let value: serde_json::Value =
            serde_json::from_str(payload).map_err(|_| "e2e status must be JSON".to_string())?;
        if !value.is_object() {
            return Err("e2e status must be a JSON object".into());
        }
        let lowered = payload.to_ascii_lowercase();
        for banned in ["password", "token", "sdp", "candidate"] {
            if lowered.contains(banned) {
                return Err(format!("e2e status must not contain '{banned}'"));
            }
        }
        let path = std::path::Path::new(&plan.status_file);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| format!("e2e status dir: {e}"))?;
            }
        }
        std::fs::write(path, payload).map_err(|e| format!("e2e status write: {e}"))?;
        Ok(())
    }

    /// Reads the room code file published by the host run. Gated on the
    /// plan; the code must look like a room code or it is rejected.
    pub fn e2e_read_code(&self) -> Result<String, String> {
        let plan = self.require_e2e_plan()?;
        let raw = std::fs::read_to_string(&plan.code_file)
            .map_err(|_| "e2e code not published yet".to_string())?;
        let code = raw.trim().to_owned();
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err("e2e code malformed".into());
        }
        Ok(code.to_uppercase())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Owner errors rendered without internals (states are enums, safe to show).
fn redact_owner(e: golive_core::owner::OwnerError) -> String {
    format!("{e:?}")
}

/// Stops spare fresh-build pieces (publisher behind its Arc + optional
/// bridge) when they lose a race (lock poisoned, share died, concurrent
/// adopt). Bounded; never resurrects anything.
async fn stop_fresh_parts(
    publisher: &Arc<tokio::sync::Mutex<Publisher>>,
    bridge: &mut Option<screen::BridgeHandle>,
) {
    publisher.lock().await.stop().await;
    if let Some(bridge) = bridge.as_mut() {
        bridge.stop();
    }
}

/// Puts taken bridge handles back on their sessions. A session that vanished
/// or turned over mid-reconfigure (fresh bridge already in place) releases
/// the orphan instead of resurrecting anything.
fn restore_bridges(inner: &mut Inner, bridges: Vec<(String, screen::BridgeHandle)>) {
    for (watcher, mut handle) in bridges {
        match inner.publishers.get_mut(&watcher) {
            Some(session) if session.bridge.is_none() => {
                session.bridge = Some(handle);
            }
            _ => {
                let _ = handle.stop();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri commands: validated pass-through only.
// ---------------------------------------------------------------------------

#[tauri::command]
async fn create_room(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    nickname: String,
    password: String,
) -> Result<String, String> {
    state.create_room(Some(app), &nickname, &password).await
}

#[tauri::command]
async fn join_room(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    code: String,
    nickname: String,
    password: String,
) -> Result<String, String> {
    state
        .join_room(Some(app), &code, &nickname, &password)
        .await
}

#[tauri::command]
async fn leave(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.leave().await
}

#[tauri::command]
async fn start_share(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    source: String,
    w: Option<u32>,
    h: Option<u32>,
    bitrate_kbps: Option<u32>,
    fps: Option<u32>,
) -> Result<(), String> {
    let profile = match (w, h, bitrate_kbps, fps) {
        (None, None, None, None) => None,
        (Some(w), Some(h), Some(bitrate_kbps), Some(fps)) => Some(QualityProfile {
            w,
            h,
            bitrate_kbps,
            fps,
        }),
        _ => return Err("qualidade inicial incompleta".into()),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let mut cancel_guard = StartupCancelGuard::new(Arc::clone(&cancel));
    let result = state
        .start_share_with_cancel(Some(app), &source, profile, cancel)
        .await;
    if result.is_ok() {
        cancel_guard.disarm();
    }
    result
}

#[tauri::command]
async fn stop_share(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.stop_share().await
}

#[tauri::command]
async fn set_quality(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    w: u32,
    h: u32,
    bitrate_kbps: u32,
    fps: u32,
    preset: Option<String>,
) -> Result<EffectiveQuality, String> {
    state
        .set_quality(
            Some(app),
            SetQualityArgs {
                w,
                h,
                bitrate_kbps,
                fps,
                preset,
            },
        )
        .await
}

#[tauri::command]
async fn list_sources(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<screen::ListedSource>, String> {
    state.list_sources().await
}

#[tauri::command]
fn source_capabilities(state: State<'_, Arc<AppState>>) -> screen::Capabilities {
    state.source_capabilities()
}

#[tauri::command]
async fn list_audio_apps(state: State<'_, Arc<AppState>>) -> Result<Vec<AudioApp>, String> {
    Ok(state.list_audio_apps().await)
}

#[tauri::command]
async fn set_audio_exclusions(
    state: State<'_, Arc<AppState>>,
    apps: Vec<String>,
) -> Result<(), String> {
    state.set_audio_exclusions(apps).await
}

#[tauri::command]
async fn watch(state: State<'_, Arc<AppState>>, member: String) -> Result<(), String> {
    let _fence = state.watch(&member).await?;
    Ok(())
}

#[tauri::command]
async fn unwatch(state: State<'_, Arc<AppState>>, member: String) -> Result<(), String> {
    state.unwatch(&member).await
}

#[tauri::command]
fn get_snapshot(state: State<'_, Arc<AppState>>) -> Result<OwnerSnapshot, String> {
    state.get_snapshot()
}

#[tauri::command]
fn get_roster(state: State<'_, Arc<AppState>>) -> Vec<RosterMember> {
    state.get_roster()
}

#[tauri::command]
async fn preview_source(
    state: State<'_, Arc<AppState>>,
    kind: String,
    id: String,
) -> Result<SourcePreview, String> {
    let owned = Arc::clone(&state);
    tokio::task::spawn_blocking(move || owned.preview_source(&kind, &id))
        .await
        .map_err(|e| format!("preview: {e}"))
}

#[tauri::command]
async fn preview_start(
    state: State<'_, Arc<AppState>>,
    kind: String,
    id: String,
    channel: Channel,
) -> Result<String, String> {
    // Device open rendezvouses (camera permission/first frame): never block
    // the async runtime on it.
    let owned = Arc::clone(&state);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut cancel_guard = StartupCancelGuard::new(Arc::clone(&cancel));
    let result = tokio::task::spawn_blocking(move || {
        owned.preview_start_with_cancel(&kind, &id, channel, cancel)
    })
    .await
    .map_err(|e| format!("preview: {e}"))?;
    if result.is_ok() {
        cancel_guard.disarm();
    }
    result
}

#[tauri::command]
async fn preview_stop(state: State<'_, Arc<AppState>>, token: String) -> Result<(), String> {
    // Stopping joins the worker (bounded ~2 s): never on the synchronous
    // UI-command path — the token shape is unchanged, and stops stay
    // idempotent (unknown tokens are Ok).
    let owned = Arc::clone(&state);
    let _ = tokio::task::spawn_blocking(move || owned.preview_stop(&token)).await;
    Ok(())
}

#[tauri::command]
async fn selfview_start(
    state: State<'_, Arc<AppState>>,
    channel: Channel,
) -> Result<String, String> {
    // Attaching the tap is instant (no device open), but keep the async
    // shape for forward-compatibility with the preview commands.
    let owned = Arc::clone(&state);
    tokio::task::spawn_blocking(move || owned.selfview_start(channel))
        .await
        .map_err(|e| format!("prévia local: {e}"))?
}

#[tauri::command]
fn selfview_stop(state: State<'_, Arc<AppState>>, token: String) {
    state.selfview_stop(&token);
}

#[tauri::command]
fn get_media_counters(state: State<'_, Arc<AppState>>) -> Result<MediaCounters, String> {
    state.get_media_counters()
}

#[tauri::command]
fn set_server(state: State<'_, Arc<AppState>>, base: String) -> Result<String, String> {
    state.set_server(&base)
}

#[tauri::command]
fn get_e2e_plan(state: State<'_, Arc<AppState>>) -> Option<E2ePlan> {
    state.get_e2e_plan()
}

#[tauri::command]
fn e2e_status(state: State<'_, Arc<AppState>>, payload: String) -> Result<(), String> {
    state.e2e_status(&payload)
}

#[tauri::command]
fn e2e_read_code(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    state.e2e_read_code()
}

/// Tauri entry point with an explicit state (tests inject their own).
pub fn run_with(state: Arc<AppState>) {
    screen::install_decoder_backend();
    let log_state = Arc::clone(&state);
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .setup(move |app| {
            if let Ok(mut inner) = log_state.inner.lock() {
                inner.desktop = Some(app.handle().clone());
            }
            match app.path().app_log_dir() {
                Ok(dir) => log_state.set_session_log(session_log::SessionLog::init_in(&dir)),
                Err(e) => eprintln!("golive: log dir unavailable: {e}"),
            }
            log_state.session_log("session start".to_string());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            player::player_attach,
            player::player_detach,
            player::player_ack,
            player::player_context,
            player::player_audio,
            player::player_mute_all,
            player::player_popup,
            create_room,
            join_room,
            leave,
            start_share,
            stop_share,
            set_quality,
            list_sources,
            source_capabilities,
            list_audio_apps,
            set_audio_exclusions,
            watch,
            unwatch,
            get_snapshot,
            get_roster,
            preview_source,
            preview_start,
            preview_stop,
            selfview_start,
            selfview_stop,
            get_media_counters,
            set_server,
            get_e2e_plan,
            e2e_status,
            e2e_read_code,
        ])
        .run(tauri::generate_context!())
        .expect("tauri runtime");
}

/// Tauri entry point. Parses `--e2e-plan '<json>'` (absent = normal app).
pub fn run() {
    let state = Arc::new(AppState::new());
    match E2ePlan::from_args(std::env::args()) {
        Ok(Some(plan)) => {
            if let Err(e) = state.set_e2e_plan(plan) {
                eprintln!("e2e plan install failed: {e}");
                std::process::exit(2);
            }
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
    run_with(state);
}

#[cfg(test)]
mod e2e_plan_tests {
    use super::*;

    fn args(extra: &[&str]) -> impl Iterator<Item = String> {
        let mut v = vec!["goDrinking".to_owned()];
        v.extend(extra.iter().map(|s| s.to_string()));
        v.into_iter()
    }

    const PLAN: &str = r#"{"role":"host","server":"http://127.0.0.1:1","password":"pw","nickname":"n","code_file":"c","status_file":"s"}"#;

    #[test]
    fn absent_flag_means_normal_app() {
        assert_eq!(E2ePlan::from_args(args(&[])).unwrap(), None);
        assert_eq!(E2ePlan::from_args(args(&["--other"])).unwrap(), None);
    }

    #[test]
    fn parses_host_and_viewer() {
        let plan = E2ePlan::from_args(args(&["--e2e-plan", PLAN]))
            .unwrap()
            .unwrap();
        assert_eq!(plan.role, "host");
        let viewer = PLAN.replace("\"host\"", "\"viewer\"");
        let plan = E2ePlan::from_args(args(&["--e2e-plan", &viewer]))
            .unwrap()
            .unwrap();
        assert_eq!(plan.role, "viewer");
    }

    #[test]
    fn e2e_quality_is_optional_and_validated() {
        let mut raw: serde_json::Value = serde_json::from_str(PLAN).unwrap();
        assert_eq!(
            E2ePlan::from_args(args(&["--e2e-plan", PLAN]))
                .unwrap()
                .unwrap()
                .quality,
            None
        );
        raw["quality"] = serde_json::json!({"w":1920,"h":1080,"bitrate_kbps":6000,"fps":60});
        let encoded = raw.to_string();
        assert_eq!(
            E2ePlan::from_args(args(&["--e2e-plan", &encoded]))
                .unwrap()
                .unwrap()
                .quality
                .unwrap()
                .fps,
            60
        );
        raw["quality"]["fps"] = serde_json::json!(0);
        assert!(E2ePlan::from_args(args(&["--e2e-plan", &raw.to_string()])).is_err());
    }

    #[test]
    fn share_defaults_to_none_and_accepts_display() {
        // Absent == synthetic (existing plans keep working unchanged).
        let plan = E2ePlan::from_args(args(&["--e2e-plan", PLAN]))
            .unwrap()
            .unwrap();
        assert_eq!(plan.share, None);
        // display:<id> passes through; empty/unknown share rejects at parse.
        let display = PLAN.replace('}', r#","share":"display:3"}"#);
        let plan = E2ePlan::from_args(args(&["--e2e-plan", &display]))
            .unwrap()
            .unwrap();
        assert_eq!(plan.share.as_deref(), Some("display:3"));
        let empty = PLAN.replace('}', r#","share":""}"#);
        assert!(E2ePlan::from_args(args(&["--e2e-plan", &empty])).is_err());
        let bogus = PLAN.replace('}', r#","share":"screen"}"#);
        assert!(E2ePlan::from_args(args(&["--e2e-plan", &bogus])).is_err());
    }

    #[test]
    fn rejects_bad_role_json_and_empty_fields() {
        assert!(E2ePlan::from_args(args(&["--e2e-plan"])).is_err());
        assert!(E2ePlan::from_args(args(&["--e2e-plan", "{}"])).is_err());
        let bad_role = PLAN.replace("\"host\"", "\"cameraman\"");
        assert!(E2ePlan::from_args(args(&["--e2e-plan", &bad_role])).is_err());
        let empty = PLAN.replace("\"n\"", "\"\"");
        assert!(E2ePlan::from_args(args(&["--e2e-plan", &empty])).is_err());
    }

    #[test]
    fn test_only_commands_need_a_plan() {
        let state = AppState::new();
        assert_eq!(state.get_e2e_plan(), None);
        assert_eq!(state.e2e_status("{}"), Err("e2e not active".to_string()));
        assert_eq!(state.e2e_read_code(), Err("e2e not active".to_string()));
    }

    #[test]
    fn status_refuses_secrets_and_non_objects() {
        let state = AppState::new();
        state
            .set_e2e_plan(
                E2ePlan::from_args(args(&["--e2e-plan", PLAN]))
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        assert!(state.e2e_status("[1,2]").is_err());
        assert!(state.e2e_status(r#"{"token":"x"}"#).is_err());
        assert!(state.e2e_status(r#"{"sdp":"v=0"}"#).is_err());
        // Clean payloads to a temp file succeed.
        let dir = std::env::temp_dir().join("golive-e2e-unit");
        let _ = std::fs::create_dir_all(&dir);
        let status = dir.join("s.json");
        let mut plan = E2ePlan::from_args(args(&["--e2e-plan", PLAN]))
            .unwrap()
            .unwrap();
        plan.status_file = status.to_string_lossy().into_owned();
        let state = AppState::new();
        state.set_e2e_plan(plan).unwrap();
        state.e2e_status(r#"{"state":"ok"}"#).unwrap();
        assert_eq!(
            std::fs::read_to_string(&status).unwrap(),
            r#"{"state":"ok"}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod preview_command_tests {
    use super::*;
    use tauri::ipc::{Channel, InvokeResponseBody};

    #[test]
    fn preview_start_rejects_unknown_kind_without_touching_os() {
        let state = Arc::new(AppState::new());
        assert!(state
            .preview_start("screen", "1", Channel::new(|_| Ok(())))
            .is_err());
        assert!(state
            .preview_start("", "", Channel::new(|_| Ok(())))
            .is_err());
        // Unknown tokens stop silently (idempotent by design).
        state.preview_stop("pv-nope");
        state.stop_all_previews();
    }

    #[test]
    #[ignore]
    fn hw_preview_command_streams_glp2_to_channel() {
        // Real webcam through the Tauri command layer (no webview involved).
        // Run explicitly, serially (single-open device):
        // cargo test -p golive-app --lib hw_preview_command -- --ignored --nocapture --test-threads=1
        use std::time::Duration;
        let listed = screen::enumerate_sources().expect("real enumerate");
        let cams: Vec<_> = listed
            .iter()
            .filter(|s| s.kind == golive_platform::SourceKind::Camera)
            .collect();
        if cams.is_empty() {
            eprintln!("no camera on this machine; skipping");
            return;
        }
        let state = Arc::new(AppState::new());
        for cam in cams {
            // A device may open yet never deliver frames (virtual cameras) —
            // only received bytes prove a previewable webcam.
            let (tx_one, rx_one) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
            let channel = Channel::new(move |body| {
                if let InvokeResponseBody::Raw(bytes) = body {
                    let _ = tx_one.try_send(bytes);
                }
                Ok(())
            });
            let token = match state.preview_start("camera", &cam.id, channel) {
                Ok(token) => token,
                Err(e) => {
                    eprintln!("preview skipping camera: {e}");
                    continue;
                }
            };
            let mut frames = Vec::new();
            let mut live = true;
            for _ in 0..3 {
                match rx_one.recv_timeout(Duration::from_secs(12)) {
                    Ok(bytes) => {
                        assert!(bytes.len() > 20 && &bytes[..4] == b"GLP2");
                        frames.push(bytes);
                    }
                    Err(e) => {
                        eprintln!("preview camera without frames: {e}");
                        live = false;
                        break;
                    }
                }
            }
            // Stop is idempotent; the sweep covers the rest.
            state.preview_stop(&token);
            state.preview_stop(&token);
            if live {
                assert_eq!(frames.len(), 3);
                state.stop_all_previews();
                return;
            }
        }
        state.stop_all_previews();
        panic!("a previewable webcam");
    }
}

#[cfg(test)]
mod share_source_tests {
    use super::*;

    #[test]
    fn parses_all_four_kinds() {
        assert!(matches!(
            ShareSource::parse("synthetic").unwrap(),
            ShareSource::Synthetic
        ));
        assert!(matches!(
            ShareSource::parse("movie:/tmp/a.mp4").unwrap(),
            ShareSource::Movie(_)
        ));
        assert!(matches!(
            ShareSource::parse("display:1").unwrap(),
            ShareSource::Display(_)
        ));
        assert!(matches!(
            ShareSource::parse("window:42").unwrap(),
            ShareSource::Window(_)
        ));
        assert!(matches!(
            ShareSource::parse("camera:0").unwrap(),
            ShareSource::Camera(_)
        ));
    }

    #[test]
    fn parses_combo_screen_plus_camera() {
        match ShareSource::parse("combo:display:1+camera:0").unwrap() {
            ShareSource::Combo { screen, camera } => {
                assert!(matches!(*screen, ShareSource::Display(_)));
                assert_eq!(camera, "0");
            }
            other => panic!("expected combo, got {other:?}"),
        }
        match ShareSource::parse("combo:window:42+camera:1").unwrap() {
            ShareSource::Combo { screen, camera } => {
                assert!(matches!(*screen, ShareSource::Window(_)));
                assert_eq!(camera, "1");
            }
            other => panic!("expected combo, got {other:?}"),
        }
        // Screen half must be display/window; camera id must exist.
        assert!(ShareSource::parse("combo:camera:0+camera:1").is_err());
        assert!(ShareSource::parse("combo:synthetic+camera:0").is_err());
        assert!(ShareSource::parse("combo:display:1+camera:").is_err());
        assert!(ShareSource::parse("combo:display:1").is_err());
        assert!(ShareSource::parse("combo:").is_err());
    }

    #[test]
    fn rejects_empty_ids_and_unknown() {
        assert!(ShareSource::parse("display:").is_err());
        assert!(ShareSource::parse("display:   ").is_err());
        assert!(ShareSource::parse("window:").is_err());
        assert!(ShareSource::parse("camera:").is_err());
        assert!(ShareSource::parse("camera:   ").is_err());
        assert!(ShareSource::parse("movie:").is_err());
        assert!(ShareSource::parse("screen").is_err());
        assert!(ShareSource::parse("").is_err());
    }

    #[test]
    fn window_share_uses_window_audio_display_does_not() {
        let window = ShareSource::parse("window:42").unwrap();
        let display = ShareSource::parse("display:\\\\.\\DISPLAY1").unwrap();
        let camera = ShareSource::parse("camera:0").unwrap();
        let combo_window = ShareSource::parse("combo:window:42+camera:0").unwrap();
        let combo_display = ShareSource::parse("combo:display:1+camera:0").unwrap();
        assert_eq!(window_audio_id(&window), Some("42"));
        assert_eq!(window_audio_id(&display), None);
        assert_eq!(window_audio_id(&camera), None);
        // Combo inherits the screen half's audio: window taps, display ducks.
        assert_eq!(window_audio_id(&combo_window), Some("42"));
        assert_eq!(window_audio_id(&combo_display), None);
        assert_eq!(window_audio_id(&ShareSource::Synthetic), None);
    }

    #[test]
    fn initial_share_profile_keeps_requested_high_not_720p() {
        let high = Quality::P1080.profile();
        let got = initial_share_profile(Some(high)).expect("high");
        assert_eq!((got.w, got.h, got.fps), (1920, 1080, 60));
        let default = initial_share_profile(None).expect("default");
        assert_eq!((default.w, default.h, default.fps), (1280, 720, 30));
        assert!(initial_share_profile(Some(QualityProfile {
            w: 1,
            h: 1,
            bitrate_kbps: 10,
            fps: 0,
        }))
        .is_err());
    }

    // NOTE: Display/Window must never resolve to synthetic silently. That
    // invariant holds structurally: start_share matches ShareSource
    // exhaustively with no wildcard arm, and both capture arms go through
    // screen::start_capture_for (OS-backed, typed errors). A silent fallback
    // cannot compile here without touching that match.
}

#[cfg(test)]
mod roster_tests {
    use super::*;

    #[test]
    fn offline_roster_pull_is_empty_never_errors() {
        // No signal client yet (fresh state): pull returns empty instead of
        // erroring — the event listener fills the roster in once WS connects.
        let state = AppState::new();
        assert!(state.get_roster().is_empty());
    }

    #[test]
    fn preview_unknown_kind_is_null_never_errors() {
        // No OS contact on this path: unknown kind short-circuits to a null
        // thumbnail (the modal lists sources regardless).
        let state = AppState::new();
        let preview = state.preview_source("bogus", "1");
        assert!(preview.data_url.is_none());
        assert_eq!((preview.w, preview.h), (0, 0));
    }
}

#[cfg(test)]
mod quality_tests {
    use super::*;
    use golive_core::media::EngineKind;
    use std::time::Duration;

    fn args(w: u32, h: u32, bitrate_kbps: u32, fps: u32) -> SetQualityArgs {
        SetQualityArgs {
            w,
            h,
            bitrate_kbps,
            fps,
            preset: None,
        }
    }

    /// Wire regression: canonical snake_case and legacy camelCase spellings
    /// deserialize to the same args struct (kept tolerant for any caller).

    /// Payload tolerance: flat snake and legacy camel spellings bind.

    /// Seeds one live software publisher (synthetic) + default effective,
    /// bypassing room/signal: `set_quality` only touches publishers.
    /// Returns the state plus the publisher's event channel (proves IDR +
    /// fence without any forward task).
    async fn live_state() -> (
        Arc<AppState>,
        tokio::sync::mpsc::UnboundedReceiver<golive_core::media::MediaEvent>,
    ) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::start_with_profile(
            VideoSource::SyntheticBall,
            Quality::P720.profile(),
            EngineKind::Software,
            None,
            event_tx,
        )
        .await
        .expect("test publisher starts");
        let state = Arc::new(AppState::new());
        {
            let mut inner = state.inner.lock().expect("state lock");
            inner.publishers.insert(
                "watcher".into(),
                PublishSession {
                    publisher: Arc::new(tokio::sync::Mutex::new(publisher)),
                    owner_fence: Fence::idle(),
                    wire: WireIds::default(),
                    remote_ready: false,
                    pending_remote: Vec::new(),
                    bridge: None,
                },
            );
            inner.share_profile = Some(EffectiveQuality {
                profile: Quality::P720.profile(),
                generation: 0,
            });
        }
        (state, event_rx)
    }

    async fn recv_timeout(
        event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<golive_core::media::MediaEvent>,
        secs: u64,
    ) -> Option<golive_core::media::MediaEvent> {
        tokio::time::timeout(Duration::from_secs(secs), event_rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn valid_applies_with_idr_and_fence_snapshot_reflects() {
        let (state, mut event_rx) = live_state().await;
        // Liveness first (initial IDR), so the post-reconfig IDR is
        // attributable below.
        let mut live = false;
        for _ in 0..150 {
            match recv_timeout(&mut event_rx, 1).await {
                Some(golive_core::media::MediaEvent::Keyframe) => {
                    live = true;
                    break;
                }
                Some(golive_core::media::MediaEvent::Error(detail)) => {
                    panic!("encode failed: {detail}")
                }
                _ => {}
            }
        }
        assert!(live, "stream alive before set_quality");
        // Valid profile applies through the command method (no AppHandle).
        let effective = state
            .set_quality(None, args(640, 360, 1000, 15))
            .await
            .expect("valid profile applies");
        assert_eq!((effective.profile.w, effective.profile.h), (640, 360));
        assert_eq!(effective.profile.bitrate_kbps, 1000);
        // Fence + forced IDR land on the publisher's own channel.
        let mut gen_seen = false;
        let mut post_idr = false;
        for _ in 0..200 {
            match recv_timeout(&mut event_rx, 1).await {
                Some(golive_core::media::MediaEvent::Stats(stats)) => {
                    if stats.generation == 1 {
                        gen_seen = true;
                    }
                }
                Some(golive_core::media::MediaEvent::Keyframe) => {
                    if gen_seen {
                        post_idr = true;
                        break;
                    }
                }
                _ => {}
            }
            if gen_seen && post_idr {
                break;
            }
        }
        assert!(gen_seen, "generation fence bumps");
        assert!(post_idr, "forced IDR after apply");
        // Snapshot reflects the effective profile (generation follows via
        // the forward task; the stored profile is authoritative here) plus
        // the live encode backend for the UI badge/diagnostics.
        let counters = state.get_media_counters().expect("counters");
        let stored = counters.effective.expect("effective present");
        assert_eq!((stored.profile.w, stored.profile.h), (640, 360));
        assert_eq!(counters.backend.as_deref(), Some("openh264"));
        // Teardown: stop the publisher explicitly (encode thread joins).
        let publisher = {
            state
                .inner
                .lock()
                .expect("state lock")
                .publishers
                .remove("watcher")
                .map(|session| session.publisher)
        };
        if let Some(publisher) = publisher {
            publisher.lock().await.stop().await;
        }
    }

    #[tokio::test]
    async fn set_quality_never_writes_generation_backwards() {
        // Race: set_quality snapshots previous (gen 0), then parks on the
        // publisher/bridge awaits while the encode loop applies the reconfig
        // and the forward task bumps share_profile.generation to 1. The
        // final store must take the max, never erase the observed bump.
        // No real bridge in unit tests (needs OS capture), so the publisher
        // mutex is held to open the same await window deterministically.
        let (state, _event_rx) = live_state().await;
        let publisher = {
            state
                .inner
                .lock()
                .expect("state lock")
                .publishers
                .get("watcher")
                .expect("watcher")
                .publisher
                .clone()
        };
        let guard = publisher.lock().await;
        let parked = Arc::clone(&state);
        let task =
            tokio::spawn(async move { parked.set_quality(None, args(640, 360, 1000, 15)).await });
        // The parked call can only have snapshotted previous (gen 0) by now:
        // it cannot proceed past the held publisher lock.
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let mut inner = state.inner.lock().expect("state lock");
            if let Some(share) = inner.share_profile.as_mut() {
                share.generation = 1;
            }
        }
        drop(guard);
        let effective = task.await.expect("join").expect("set_quality applies");
        assert!(
            effective.generation >= 1,
            "stale snapshot must not clobber the observed bump (got {})",
            effective.generation
        );
        let stored = state
            .get_media_counters()
            .expect("counters")
            .effective
            .expect("effective present");
        assert_eq!((stored.profile.w, stored.profile.h), (640, 360));
        assert!(
            stored.generation >= 1,
            "stored generation keeps the bump (got {})",
            stored.generation
        );
        // Teardown: stop the publisher explicitly (encode thread joins).
        let publisher = {
            state
                .inner
                .lock()
                .expect("state lock")
                .publishers
                .remove("watcher")
                .map(|session| session.publisher)
        };
        if let Some(publisher) = publisher {
            publisher.lock().await.stop().await;
        }
    }

    #[tokio::test]
    async fn invalid_rejects_without_touching_stream() {
        let (state, _event_rx) = live_state().await;
        // Odd dims are ACCEPTED (core floors to even in the encoder); only
        // ranges + unknown presets reject. Errors are typed + redacted.
        let odd = state
            .set_quality(None, args(641, 360, 1000, 15))
            .await
            .expect("odd dims accepted, normalized downstream");
        assert_eq!(odd.profile.w, 641);
        // Out-of-range bitrate/fps + unknown preset reject without touching
        // the stream.
        let err = state
            .set_quality(None, args(640, 360, 50, 15))
            .await
            .expect_err("bitrate range rejected");
        assert!(err.starts_with("qualidade:"), "{err}");
        assert!(state
            .set_quality(None, args(640, 360, 50, 15))
            .await
            .is_err());
        assert!(state
            .set_quality(None, args(640, 360, 1000, 0))
            .await
            .is_err());
        assert!(state
            .set_quality(
                None,
                SetQualityArgs {
                    preset: Some("ultra".into()),
                    ..args(640, 360, 1000, 15)
                }
            )
            .await
            .is_err());
        // Effective tracks the last ACCEPTED profile (odd included); the
        // rejects above left it untouched, and the encoder still runs:
        // a later valid switch applies cleanly.
        let stored = state
            .get_media_counters()
            .expect("counters")
            .effective
            .expect("effective present");
        assert_eq!((stored.profile.w, stored.profile.h), (641, 360));
        assert_eq!(stored.generation, 0);
        state
            .set_quality(None, args(480, 270, 800, 15))
            .await
            .expect("encoder alive: later valid applies");
        let stored = state
            .get_media_counters()
            .expect("counters")
            .effective
            .expect("effective present");
        assert_eq!((stored.profile.w, stored.profile.h), (480, 270));
        let publisher = {
            state
                .inner
                .lock()
                .expect("state lock")
                .publishers
                .remove("watcher")
                .map(|session| session.publisher)
        };
        if let Some(publisher) = publisher {
            publisher.lock().await.stop().await;
        }
    }

    #[tokio::test]
    async fn rejects_when_not_sharing() {
        let state = Arc::new(AppState::new());
        let err = state
            .set_quality(None, args(640, 360, 1000, 15))
            .await
            .expect_err("no publishers");
        assert_eq!(err, "not sharing");
        assert!(state
            .get_media_counters()
            .expect("counters")
            .effective
            .is_none());
    }

    #[test]
    fn backend_note_triages_fallback_deterministically() {
        // Hardware and absence carry no note.
        assert_eq!(backend_note_for(None), None);
        assert_eq!(backend_note_for(Some("videotoolbox")), None);
        assert_eq!(backend_note_for(Some("nvenc")), None);
        assert_eq!(backend_note_for(Some("whatever")), None);
        // Software fallback always explains itself, never with secrets.
        let hook_was_set = std::env::var_os("GOLIVE_DISABLE_HW").is_some();
        std::env::set_var("GOLIVE_DISABLE_HW", "1");
        let hooked = backend_note_for(Some("openh264")).expect("note");
        assert!(hooked.contains("GOLIVE_DISABLE_HW"), "{hooked}");
        if hook_was_set {
            std::env::set_var("GOLIVE_DISABLE_HW", "1");
        } else {
            std::env::remove_var("GOLIVE_DISABLE_HW");
        }
        let plain = backend_note_for(Some("openh264")).expect("note");
        assert!(!plain.is_empty());
        for note in [hooked, plain] {
            let lower = note.to_lowercase();
            for banned in ["sdp", "candidate", "token", "password", "192.168"] {
                assert!(!lower.contains(banned), "secret-adjacent in note: {note}");
            }
        }
    }
}

#[cfg(test)]
mod operation_tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    #[tokio::test]
    async fn unwatch_cannot_clear_fences_during_another_operation() {
        let state = Arc::new(AppState::new());
        {
            let mut inner = state.inner.lock().unwrap();
            let join = inner.owner.begin_join().unwrap();
            inner.owner.complete_opened(&join).unwrap();
            let fence = inner.owner.watch_remote("ana").unwrap();
            inner.viewers.insert(
                "ana".into(),
                WatchSession {
                    playback: None,
                    fence,
                    viewer: None,
                    adopted: None,
                    alive: None,
                    remote_ready: false,
                    pending_remote: Vec::new(),
                },
            );
        }
        let operation = state.operations.lock().await;
        let mut unwatch = Box::pin(state.unwatch("ana"));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(unwatch.as_mut().poll(&mut context), Poll::Pending));
        assert!(state.inner.lock().unwrap().viewers.contains_key("ana"));
        drop(operation);
        unwatch.await.unwrap();
        assert!(!state.inner.lock().unwrap().viewers.contains_key("ana"));
    }

    #[tokio::test]
    async fn watch_two_hosts_keeps_independent_sessions() {
        let state = Arc::new(AppState::new());
        {
            let mut inner = state.inner.lock().unwrap();
            let join = inner.owner.begin_join().unwrap();
            inner.owner.complete_opened(&join).unwrap();
            for host in ["host-a", "host-b"] {
                let fence = inner.owner.watch_remote(host).unwrap();
                inner.viewers.insert(
                    host.into(),
                    WatchSession {
                        playback: None,
                        fence,
                        viewer: None,
                        adopted: None,
                        alive: None,
                        remote_ready: false,
                        pending_remote: Vec::new(),
                    },
                );
            }
        }
        {
            let inner = state.inner.lock().unwrap();
            assert_eq!(inner.viewers.len(), 2);
            assert!(inner.viewers.contains_key("host-a"));
            assert!(inner.viewers.contains_key("host-b"));
            let mut names = inner.owner.watchers();
            names.sort();
            assert_eq!(names, vec!["host-a".to_owned(), "host-b".to_owned()]);
        }
        state.unwatch("host-a").await.unwrap();
        {
            let inner = state.inner.lock().unwrap();
            assert!(!inner.viewers.contains_key("host-a"));
            assert!(inner.viewers.contains_key("host-b"));
            assert_eq!(inner.owner.watchers(), vec!["host-b".to_owned()]);
        }
    }

    #[test]
    fn watch_session_dying_only_counts_explicit_teardown() {
        // The re-watch replace path must fire exactly for sessions the
        // window-death path abandoned — never for fresh intents (alive None,
        // pre-adoption) or live sessions.
        fn session(alive: Option<bool>) -> WatchSession {
            WatchSession {
                playback: None,
                fence: Fence::idle(),
                viewer: None,
                adopted: None,
                alive: alive.map(|flag| Arc::new(std::sync::atomic::AtomicBool::new(flag))),
                remote_ready: false,
                pending_remote: Vec::new(),
            }
        }
        assert!(
            !watch_session_is_dying(&session(None)),
            "fresh intent is not dying"
        );
        assert!(
            !watch_session_is_dying(&session(Some(true))),
            "live session is not dying"
        );
        assert!(
            watch_session_is_dying(&session(Some(false))),
            "torn-down session is dying"
        );
    }

    #[tokio::test]
    async fn unwatch_one_host_does_not_wipe_the_other_session_or_counters() {
        let state = Arc::new(AppState::new());
        {
            let mut inner = state.inner.lock().unwrap();
            let join = inner.owner.begin_join().unwrap();
            inner.owner.complete_opened(&join).unwrap();
            for host in ["host-a", "host-b"] {
                let fence = inner.owner.watch_remote(host).unwrap();
                inner.viewers.insert(
                    host.into(),
                    WatchSession {
                        playback: None,
                        fence,
                        viewer: None,
                        adopted: Some(WireIds {
                            session: "1".into(),
                            share: "2".into(),
                            link: host.into(),
                            attempt: "1".into(),
                        }),
                        alive: None,
                        remote_ready: true,
                        pending_remote: vec!["cand".into()],
                    },
                );
            }
            inner.media_counters.connected = true;
            inner.media_counters.frames = 40;
        }
        state.unwatch("host-a").await.unwrap();
        let inner = state.inner.lock().unwrap();
        let other = inner.viewers.get("host-b").expect("host-b remains");
        assert_eq!(
            other.adopted.as_ref().map(|ids| ids.link.as_str()),
            Some("host-b")
        );
        assert!(other.remote_ready);
        assert_eq!(other.pending_remote.len(), 1);
        assert!(inner.media_counters.connected);
        assert_eq!(inner.media_counters.frames, 40);
    }
}

#[cfg(test)]
mod selfview_resubscribe_tests {
    use super::*;

    fn i420(w: usize, h: usize, fill: u8) -> golive_core::media::I420Frame {
        golive_core::media::I420Frame {
            w,
            h,
            data: vec![fill; w * h * 3 / 2],
        }
    }

    #[test]
    fn pack_selfview_frame_is_glp2_format1_and_skips_odd() {
        let bytes = pack_selfview_frame(&i420(4, 2, 7), 9).expect("even packs");
        assert_eq!(&bytes[..4], b"GLP2");
        assert_eq!(&bytes[4..8], &9u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &4u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &2u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1u32.to_le_bytes(), "self-view is I420");
        assert_eq!(&bytes[20..], vec![7u8; 12].as_slice());
        // Odd dims and bad lengths skip (frontend rejects odd the same way).
        assert!(pack_selfview_frame(&i420(3, 2, 0), 0).is_none());
        let mut bad = i420(4, 2, 0);
        bad.data.pop();
        assert!(pack_selfview_frame(&bad, 0).is_none());
    }

    #[test]
    fn forwarder_survives_bridge_replacement_with_continuous_seq() {
        // Bridge 1 delivers one frame, then is replaced: the forwarder picks
        // up bridge 2's feed on its own, seq continuing (one tile stream).
        let (tx1, rx1) = std::sync::mpsc::channel();
        tx1.send(i420(2, 2, 10)).unwrap();
        drop(tx1);
        let (tx2, rx2) = std::sync::mpsc::channel();
        tx2.send(i420(2, 2, 20)).unwrap();
        drop(tx2);
        let mut slot = Some(rx2);
        let mut reattach = move || slot.take();
        let stop = AtomicBool::new(false);
        let mut sent = Vec::new();
        let mut send = |bytes: Vec<u8>| {
            sent.push(bytes);
            true
        };
        forward_selfview(
            rx1,
            &stop,
            std::time::Duration::from_millis(200),
            &mut reattach,
            &mut send,
        );
        assert_eq!(sent.len(), 2, "both bridges delivered");
        assert_eq!(&sent[0][4..8], &0u32.to_le_bytes());
        assert_eq!(&sent[1][4..8], &1u32.to_le_bytes(), "seq continues");
        for bytes in &sent {
            assert_eq!(&bytes[..4], b"GLP2");
            assert_eq!(&bytes[16..20], &1u32.to_le_bytes());
        }
        // Payloads prove the handoff: bridge 1 filled 10s, bridge 2 20s.
        assert!(sent[0][20..].iter().all(|b| *b == 10));
        assert!(sent[1][20..].iter().all(|b| *b == 20));
    }

    #[test]
    fn forwarder_ends_on_stop_without_a_bridge() {
        // Share ended (no bridge to reattach) and the tile hid: prompt end,
        // nothing shipped.
        let (_tx, rx) = std::sync::mpsc::channel::<golive_core::media::I420Frame>();
        let stop = AtomicBool::new(true);
        let mut reattach = || None;
        let mut sent = Vec::new();
        let mut send = |bytes: Vec<u8>| {
            sent.push(bytes);
            true
        };
        forward_selfview(
            rx,
            &stop,
            std::time::Duration::from_secs(5),
            &mut reattach,
            &mut send,
        );
        assert!(sent.is_empty());
    }

    #[test]
    fn forwarder_ends_when_downstream_dies() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(i420(2, 2, 1)).unwrap();
        let stop = AtomicBool::new(false);
        let mut reattach = || None;
        let mut send = |_: Vec<u8>| false;
        forward_selfview(
            rx,
            &stop,
            std::time::Duration::from_millis(10),
            &mut reattach,
            &mut send,
        );
    }
}

#[cfg(test)]
mod pending_preview_tests {
    use super::*;

    #[test]
    fn sweep_cancels_pending_authorization_before_late_grant_can_open() {
        let registry = Arc::new(Mutex::new(PreviewRegistry::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let token = "pending-preview".to_owned();
        registry
            .lock()
            .unwrap()
            .register(token.clone(), Arc::clone(&cancel));
        let (auth_tx, auth_rx) = std::sync::mpsc::channel::<bool>();
        let opens = Arc::new(AtomicU64::new(0));
        let previews = Arc::new(AtomicU64::new(0));
        let worker_registry = Arc::clone(&registry);
        let worker_cancel = Arc::clone(&cancel);
        let worker_opens = Arc::clone(&opens);
        let worker_previews = Arc::clone(&previews);
        let worker_token = token.clone();
        let worker = std::thread::spawn(move || {
            assert!(auth_rx.recv().unwrap());
            let may_start = worker_registry
                .lock()
                .unwrap()
                .is_pending(&worker_token, &worker_cancel);
            if !may_start {
                return;
            }
            worker_opens.fetch_add(1, Ordering::Relaxed);
            worker_previews.fetch_add(1, Ordering::Relaxed);
        });
        // Startup is paused at its authorization callback while leave/sweep
        // removes and cancels the pending registration.
        let handles = registry.lock().unwrap().sweep();
        assert!(handles.is_empty());
        auth_tx.send(true).unwrap(); // delayed grant
        worker.join().unwrap();
        assert_eq!(opens.load(Ordering::Relaxed), 0);
        assert_eq!(previews.load(Ordering::Relaxed), 0);
        assert!(cancel.load(Ordering::Acquire));
    }

    #[test]
    fn cancel_after_readiness_before_promotion_disposes_stale_handle() {
        let mut registry = PreviewRegistry::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let token = "ready-but-swept".to_owned();
        registry.register(token.clone(), Arc::clone(&cancel));
        let handle = screen::preview_handle_for_test(Arc::clone(&cancel));
        let handles = registry.sweep();
        assert!(handles.is_empty());
        let mut stale_handle = match registry.promote(&token, &cancel, handle) {
            Ok(()) => panic!("swept preview must not become active"),
            Err(handle) => handle,
        };
        // The production caller owns and tears down the rejected handle after
        // releasing the registry lock.
        assert!(stale_handle.stop().is_ok());
        assert!(!registry.is_active(&token, &cancel));
    }

    #[test]
    fn take_returns_handle_without_joining_worker() {
        // take() marks cancel and hands the handle out; the bounded join
        // happens in stop(), outside any registry lock — take itself must
        // never block on the worker. The test handle only exits once its
        // stop flag is set, so a join inside take would hang this test; a
        // prompt return plus a successful outside stop pins the split.
        let mut registry = PreviewRegistry::default();
        let cancel = Arc::new(AtomicBool::new(false));
        registry.register("pv-1".into(), Arc::clone(&cancel));
        let handle = screen::preview_handle_for_test(Arc::clone(&cancel));
        registry
            .promote("pv-1", &cancel, handle)
            .ok()
            .expect("promotion");
        let start = std::time::Instant::now();
        let mut handle = registry.take("pv-1").expect("handle out");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "take must not join the worker"
        );
        assert!(cancel.load(Ordering::Acquire));
        assert!(!registry.is_active("pv-1", &cancel));
        assert!(handle.stop().is_ok());
    }

    #[test]
    fn sweeping_active_and_pending_previews_cancels_both_and_returns_handles() {
        let mut registry = PreviewRegistry::default();
        let active_cancel = Arc::new(AtomicBool::new(false));
        let active_token = "active".to_owned();
        registry.register(active_token.clone(), Arc::clone(&active_cancel));
        let handle = screen::preview_handle_for_test(Arc::clone(&active_cancel));
        registry
            .promote(&active_token, &active_cancel, handle)
            .ok()
            .expect("promotion");
        let pending_cancel = Arc::new(AtomicBool::new(false));
        registry.register("pending".into(), Arc::clone(&pending_cancel));
        let handles = registry.sweep();
        assert_eq!(handles.len(), 1);
        assert!(active_cancel.load(Ordering::Acquire));
        assert!(pending_cancel.load(Ordering::Acquire));
        assert!(!registry.is_active(&active_token, &active_cancel));
        for mut handle in handles {
            assert!(handle.stop().is_ok());
        }
    }
}

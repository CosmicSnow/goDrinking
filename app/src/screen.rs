//! Screen-capture bridge: platform stream → core frames.
//!
//! The app owns this glue because it alone depends on both
//! `golive-platform` and `golive-core` (neither knows the other):
//! - enumerate sources / report capabilities (UI),
//! - open + start the selected source (may show the OS prompt on first use),
//! - pump packets into core frames: CPU packets downscale + convert here,
//!   GPU packets forward retained (zero-copy submit downstream).
//!
//! CPU economy (the capture sink matters as much as the encoder):
//! - capture fps is clamped to the profile fps BEFORE conversion: surplus
//!   packets die on arrival (timestamp gate, no pixel touched);
//! - CPU frames are downscaled to the profile target BEFORE conversion, so
//!   `bgra_to_i420` costs target pixels, never capture pixels;
//! - GPU packets are NEVER converted here: they ride retained into the
//!   core, which submits them to VideoToolbox with zero CPU copy (one
//!   conversion only on the software/drift fallback);
//! - the live profile is shared (`Arc<Mutex<…>>`) so `set_quality`
//!   re-clamps mid-share (plus a transactional stream restart).
//!
//! Failures are typed (`PlatformError` Display is user-safe); the bridge
//! never logs titles, pixels, or tokens.

use golive_core::media::{normalize_dims, ExternalFrame, I420Frame, QualityProfile};
use golive_core::trace::{Sample as TraceSample, Stage, Trace};
use golive_platform::{
    BgraFrame, CaptureConfig, CapturePacket, FrameStream, GpuPixelBuffer, NextError, PixelFormat,
    PlatformError, RestartOrder, SourceInfo, SourceKind, VideoSource,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Pacing tick for the bridge pump (matches the ~30fps contract loosely;
/// the core re-paces by frame counter anyway).
const BRIDGE_TICK: Duration = Duration::from_millis(100);
/// Latest-only channel depth: one in flight, one waiting.
const CHANNEL_DEPTH: usize = 2;

/// Handle to a running bridge. `stop()` is idempotent and bounded; `Drop`
/// stops best-effort. `reconfigure()` restarts the OS stream at a new
/// profile without dropping the core channel (the core repeats its last
/// frame across the gap) — new-first where the backend tolerates concurrent
/// streams, stop-first where it does not (see [`RestartOrder`]).
pub struct BridgeHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    info: SourceInfo,
    core_tx: mpsc::SyncSender<ExternalFrame>,
    live: Arc<Mutex<QualityProfile>>,
    order: RestartOrder,
    /// Set when this bridge composites screen + webcam (PiP): `info` is the
    /// screen, this is the camera. `reconfigure` restarts both, stop-first.
    combo_camera: Option<SourceInfo>,
    /// Stage self-view tap: mirrors forwarded CPU frames to the local tile.
    /// Shared with pump threads; survives `reconfigure` (same handle).
    tap: FrameTap,
}

/// Local self-view tap: mirrors forwarded CPU frames to zero or one
/// attached viewer (the stage self tile). Latest-only (cap 2, skip on
/// full); attaching replaces any previous tap; a dead receiver clears
/// itself on the next send. `Clone` shares one tap across the handle and
/// every pump thread that feeds it.
#[derive(Clone, Default)]
pub struct FrameTap {
    inner: Arc<Mutex<Option<mpsc::SyncSender<I420Frame>>>>,
}

impl FrameTap {
    /// Attaches a receiver, replacing any previous one (its side then
    /// observes disconnect). Returns the tap feed.
    pub fn attach(&self) -> mpsc::Receiver<I420Frame> {
        let (tx, rx) = mpsc::sync_channel::<I420Frame>(CHANNEL_DEPTH);
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(tx);
        }
        rx
    }

    /// Whether a viewer is currently attached (the overlay/tap decision
    /// reads this without taking the tap lock twice).
    pub fn has_viewer(&self) -> bool {
        self.inner
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// Best-effort mirror of one forwarded frame. Never blocks, never
    /// fails the share: full drops, dead clears.
    pub fn send(&self, frame: &I420Frame) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if let Some(tx) = guard.as_ref() {
            if matches!(
                tx.try_send(frame.clone()),
                Err(mpsc::TrySendError::Disconnected(_))
            ) {
                *guard = None;
            }
        }
    }

    #[cfg(test)]
    fn is_attached(&self) -> bool {
        self.has_viewer()
    }
}

impl BridgeHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Attaches the stage self-view tap to this bridge's live feed (zero or
    /// one attached viewer; re-attaching orphans the previous receiver).
    pub fn attach_tap(&self) -> mpsc::Receiver<I420Frame> {
        self.tap.attach()
    }

    /// Stream restart at `profile` on the same core channel (latest-only —
    /// no leak). The sequencing follows [`RestartOrder`]: NewFirst starts
    /// the replacement first and only then retires the old stream (a spawn
    /// failure leaves the old stream running); StopFirst stops + joins the
    /// old stream first (a spawn failure leaves no stream — the typed `Err`
    /// rolls publishers back upstream, same as today; no resurrection).
    /// Bounded (stream start rendezvous has a deadline).
    pub fn reconfigure(&mut self, profile: QualityProfile) -> Result<(), PlatformError> {
        // Combo (screen + webcam PiP) restarts both OS streams, always
        // stop-first: DXGI allows one duplication per output and a webcam
        // reopen races driver teardown. A spawn failure leaves no stream —
        // the typed `Err` rolls publishers back upstream, same as single.
        if let Some(camera) = self.combo_camera.clone() {
            self.stop.store(true, Ordering::Release);
            if let Some(old) = self.thread.take() {
                let _ = old.join();
            }
            let screen_config = profile_config(&self.info, &profile);
            let camera_config = profile_config(&camera, &profile);
            let (thread, stop) = spawn_composite_stream(
                self.info.clone(),
                camera.clone(),
                screen_config,
                camera_config,
                self.core_tx.clone(),
                Arc::clone(&self.live),
                self.tap.clone(),
                Arc::new(AtomicBool::new(false)),
            )?;
            self.thread = Some(thread);
            self.stop = stop;
            self.combo_camera = Some(camera);
            return Ok(());
        }
        let config = profile_config(&self.info, &profile);
        let order = self.order;
        // Disjoint field captures: the retire closure owns stop/thread, the
        // spawn closure only reads info/channel/profile.
        let (thread, stop, _) = restart(
            order,
            || {
                self.stop.store(true, Ordering::Release);
                if let Some(old) = self.thread.take() {
                    let _ = old.join();
                }
            },
            || {
                spawn_stream(
                    self.info.clone(),
                    config,
                    self.core_tx.clone(),
                    Arc::clone(&self.live),
                    self.tap.clone(),
                )
            },
        )?;
        self.thread = Some(thread);
        self.stop = stop;
        Ok(())
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Restart sequencing contract (the hook-level seam `reconfigure` runs
/// through): [`RestartOrder::NewFirst`] spawns the replacement first and
/// only retires the old stream once the new one is live, so a spawn failure
/// leaves the old stream untouched; [`RestartOrder::StopFirst`] retires the
/// old stream first, so a spawn failure propagates with no stream left (the
/// caller rolls back loudly instead of resurrecting). Pure ordering — the OS
/// calls happen in the closures.
fn restart<T, E>(
    order: RestartOrder,
    stop_old: impl FnOnce(),
    spawn_new: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    match order {
        RestartOrder::NewFirst => {
            let built = spawn_new()?;
            stop_old();
            Ok(built)
        }
        RestartOrder::StopFirst => {
            stop_old();
            spawn_new()
        }
    }
}

/// Profile-sized output config for a source: fit the source into the
/// profile (aspect preserved, never upscale), then clamp to backend
/// bounds. The SCK stream scales in capture, so the callback never sees
/// full-res pixels.
fn profile_config(info: &SourceInfo, profile: &QualityProfile) -> CaptureConfig {
    let (tw, th) = if info.w >= 2 && info.h >= 2 {
        normalize_dims(info.w, info.h, profile.w, profile.h)
    } else {
        (profile.w, profile.h)
    };
    golive_platform::capture_config_for(tw, th, profile.fps)
}

/// Spawns one OS stream + pump feeding `core_tx`. `open_stream` only
/// returns once the backend rendezvoused started, so no second rendezvous
/// here; thread-builder failure is the only spawn error. Shared by initial
/// start and reconfigure. Passes the backend's [`RestartOrder`] through so
/// the handle stores it once (queried at the selection point in
/// `open_stream`, never re-queried per switch).
fn spawn_stream(
    info: SourceInfo,
    config: CaptureConfig,
    core_tx: mpsc::SyncSender<ExternalFrame>,
    live: Arc<Mutex<QualityProfile>>,
    tap: FrameTap,
) -> Result<(std::thread::JoinHandle<()>, Arc<AtomicBool>, RestartOrder), PlatformError> {
    let stop = Arc::new(AtomicBool::new(false));
    spawn_stream_with_stop(info, config, core_tx, live, tap, stop)
}

fn spawn_stream_with_stop(
    info: SourceInfo,
    config: CaptureConfig,
    core_tx: mpsc::SyncSender<ExternalFrame>,
    live: Arc<Mutex<QualityProfile>>,
    tap: FrameTap,
    stop: Arc<AtomicBool>,
) -> Result<(std::thread::JoinHandle<()>, Arc<AtomicBool>, RestartOrder), PlatformError> {
    let (mut stream, order) = open_stream(&info, &config, Arc::clone(&stop))?;
    let stop_ = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("golive-screen-bridge".into())
        .spawn(move || {
            pump_bridge(
                &mut stream,
                &core_tx,
                &stop_,
                &live,
                Instant::now,
                &tap,
                &materialize_gpu,
            );
        })
        .map_err(|e| PlatformError::Internal(format!("thread da ponte: {e}")))?;
    Ok((thread, stop, order))
}

/// Starts the OS stream for `info` and bridges it into core frames.
/// Returns the core channel plus the handle that owns the backend.
/// Errors before any lifecycle is touched (pre-flight safe).
/// `live` carries the current share profile (fps clamp + target dims);
/// `set_quality` re-clamps it and restarts the stream via `reconfigure`.
pub fn start_capture(
    info: &SourceInfo,
    config: CaptureConfig,
    live: Arc<Mutex<QualityProfile>>,
) -> Result<(mpsc::Receiver<ExternalFrame>, BridgeHandle), PlatformError> {
    start_capture_with_cancel(info, config, live, Arc::new(AtomicBool::new(false)))
}

pub fn start_capture_with_cancel(
    info: &SourceInfo,
    config: CaptureConfig,
    live: Arc<Mutex<QualityProfile>>,
    cancel: Arc<AtomicBool>,
) -> Result<(mpsc::Receiver<ExternalFrame>, BridgeHandle), PlatformError> {
    let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(CHANNEL_DEPTH);
    let tap = FrameTap::default();
    let stop = Arc::clone(&cancel);
    let (thread, stop, order) = spawn_stream_with_stop(
        info.clone(),
        config,
        core_tx.clone(),
        Arc::clone(&live),
        tap.clone(),
        stop,
    )?;
    Ok((
        core_rx,
        BridgeHandle {
            stop,
            thread: Some(thread),
            info: info.clone(),
            core_tx,
            live,
            order,
            combo_camera: None,
            tap,
        },
    ))
}

/// Starts OS capture for one listed source id and bridges it into core
/// frames. Pre-flight safe: on error nothing was started and nothing leaks.
/// `label` uses kind+id only — never the user-facing name (titles stay out
/// of logs). Capture asks the OS for at most the profile fps (clamped to
/// the backend's 1..=60 range); the bridge gate below enforces it exactly.
pub fn start_capture_for(
    kind: SourceKind,
    id: &str,
    profile: QualityProfile,
    live: Arc<Mutex<QualityProfile>>,
) -> Result<(mpsc::Receiver<ExternalFrame>, BridgeHandle, String), PlatformError> {
    start_capture_for_with_cancel(kind, id, profile, live, Arc::new(AtomicBool::new(false)))
}

pub fn start_capture_for_with_cancel(
    kind: SourceKind,
    id: &str,
    profile: QualityProfile,
    live: Arc<Mutex<QualityProfile>>,
    cancel: Arc<AtomicBool>,
) -> Result<(mpsc::Receiver<ExternalFrame>, BridgeHandle, String), PlatformError> {
    let info = selected_source_info(kind, id)?;
    let label = match kind {
        SourceKind::Display => format!("display:{id}"),
        SourceKind::Window => format!("window:{id}"),
        SourceKind::Camera => format!("camera:{id}"),
    };
    let config = profile_config(&info, &profile);
    let (rx, handle) = start_capture_with_cancel(&info, config, live, cancel)?;
    Ok((rx, handle, label))
}

fn selected_source_info(kind: SourceKind, id: &str) -> Result<SourceInfo, PlatformError> {
    #[cfg(target_os = "macos")]
    if kind == SourceKind::Camera {
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(PlatformError::InvalidSource {
                reason: "id de webcam inválido",
            });
        }
        // AVFoundation authorization belongs to start_with_cancel, not
        // discovery. Keep a numeric selection intact even when enumeration
        // is unavailable so explicit capture can return typed permission
        // denial instead of silently converting it to SourceGone.
        return Ok(SourceInfo {
            kind,
            id: id.to_owned(),
            name: "Webcam".into(),
            w: 1280,
            h: 720,
        });
    }
    enumerate_sources_kind(kind)?
        .into_iter()
        .find(|item| item.kind == kind && item.id == id)
        .ok_or_else(|| PlatformError::SourceGone { id: id.to_owned() })
}

/// Starts a screen + webcam composite share (webcam as a corner overlay).
/// One publisher feed, protocol-unchanged: the core and viewers see a
/// single External stream. Pre-flight safe like `start_capture_for`.
/// `label` mirrors the share descriptor (`combo:display:<id>+camera:<cid>`).
pub fn start_capture_combo(
    screen_kind: SourceKind,
    screen_id: &str,
    camera_id: &str,
    profile: QualityProfile,
    live: Arc<Mutex<QualityProfile>>,
) -> Result<(mpsc::Receiver<ExternalFrame>, BridgeHandle, String), PlatformError> {
    start_capture_combo_with_cancel(
        screen_kind,
        screen_id,
        camera_id,
        profile,
        live,
        Arc::new(AtomicBool::new(false)),
    )
}

pub fn start_capture_combo_with_cancel(
    screen_kind: SourceKind,
    screen_id: &str,
    camera_id: &str,
    profile: QualityProfile,
    live: Arc<Mutex<QualityProfile>>,
    cancel: Arc<AtomicBool>,
) -> Result<(mpsc::Receiver<ExternalFrame>, BridgeHandle, String), PlatformError> {
    if !matches!(screen_kind, SourceKind::Display | SourceKind::Window) {
        return Err(PlatformError::InvalidSource {
            reason: "combo exige tela ou janela",
        });
    }
    if camera_id.trim().is_empty() {
        return Err(PlatformError::InvalidSource {
            reason: "combo exige webcam",
        });
    }
    let listed_screen = enumerate_sources_kind(screen_kind)?;
    let screen = listed_screen
        .iter()
        .find(|item| item.kind == screen_kind && item.id == screen_id)
        .ok_or_else(|| PlatformError::SourceGone {
            id: screen_id.to_owned(),
        })?
        .clone();
    let camera = selected_source_info(SourceKind::Camera, camera_id)?;
    let screen_tag = match screen_kind {
        SourceKind::Display => "display",
        SourceKind::Window => "window",
        SourceKind::Camera => unreachable!("filtrado acima"),
    };
    let label = format!("combo:{screen_tag}:{screen_id}+camera:{camera_id}");
    let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(CHANNEL_DEPTH);
    let tap = FrameTap::default();
    let screen_config = profile_config(&screen, &profile);
    let camera_config = profile_config(&camera, &profile);
    let (thread, stop) = spawn_composite_stream(
        screen.clone(),
        camera.clone(),
        screen_config,
        camera_config,
        core_tx.clone(),
        Arc::clone(&live),
        tap.clone(),
        cancel,
    )?;
    Ok((
        core_rx,
        BridgeHandle {
            stop,
            thread: Some(thread),
            info: screen,
            core_tx,
            live,
            order: RestartOrder::StopFirst,
            combo_camera: Some(camera),
            tap,
        },
        label,
    ))
}

/// Opens both OS streams for a composite share and pumps them through one
/// compositor thread. `open_stream` rendezvouses each start, so no second
/// rendezvous here; a second-open failure closes the first (pre-flight).
fn spawn_composite_stream(
    screen: SourceInfo,
    camera: SourceInfo,
    screen_config: CaptureConfig,
    camera_config: CaptureConfig,
    core_tx: mpsc::SyncSender<ExternalFrame>,
    live: Arc<Mutex<QualityProfile>>,
    tap: FrameTap,
    cancel: Arc<AtomicBool>,
) -> Result<(std::thread::JoinHandle<()>, Arc<AtomicBool>), PlatformError> {
    let stop = Arc::clone(&cancel);
    let (mut screen_stream, _) = open_stream(&screen, &screen_config, Arc::clone(&stop))?;
    let (mut camera_stream, _) = match open_stream(&camera, &camera_config, Arc::clone(&stop)) {
        Ok(opened) => opened,
        Err(error) => {
            screen_stream.stop(Duration::from_secs(2)).ok();
            return Err(error);
        }
    };
    let stop_ = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("golive-screen-camera-combo".into())
        .spawn(move || {
            pump_composite(
                &mut screen_stream,
                &mut camera_stream,
                &core_tx,
                &stop_,
                &live,
                &tap,
                &materialize_gpu,
            );
            screen_stream.stop(Duration::from_secs(2)).ok();
            camera_stream.stop(Duration::from_secs(2)).ok();
        })
        .map_err(|e| PlatformError::Internal(format!("thread da ponte: {e}")))?;
    Ok((thread, stop))
}

/// Latest webcam frame retained across compositor iterations (pure state).
/// One camera frame keeps overlaying every accepted screen frame until the
/// camera explicitly ends or fails (which clears it); timeouts and
/// undecodable GPU packets keep the previous frame — the screen never flaps
/// between PiP and bare on a quiet camera.
#[derive(Default)]
struct CameraRetainer {
    last: Option<BgraFrame>,
    dead: bool,
}

impl CameraRetainer {
    fn push_cpu(&mut self, frame: BgraFrame) {
        self.last = Some(frame);
    }

    fn push_gpu(
        &mut self,
        gpu: &GpuPixelBuffer,
        materialize: &dyn Fn(&GpuPixelBuffer) -> Option<BgraFrame>,
    ) {
        if let Some(frame) = materialize(gpu) {
            self.last = Some(frame);
        }
    }

    fn note_end(&mut self) {
        self.dead = true;
        self.last = None;
    }

    fn frame(&self) -> Option<&BgraFrame> {
        self.last.as_ref()
    }

    fn dead(&self) -> bool {
        self.dead
    }
}

/// Composite pump: latest screen frame + latest webcam frame → one feed.
/// The webcam is a corner overlay (see `overlay_pip`); when it is missing
/// or dead the screen flows alone (degraded, never wedged). GPU screen
/// packets materialize one CPU readback when the overlay and/or the
/// self-view tap need bytes, and ride retained otherwise (zero-copy
/// preserved); a failed readback degrades to retained. Ends when the
/// screen ends/fails or `stop` fires.
fn pump_composite(
    screen: &mut FrameStream,
    camera: &mut FrameStream,
    core_tx: &mpsc::SyncSender<ExternalFrame>,
    stop: &AtomicBool,
    live: &Arc<Mutex<QualityProfile>>,
    tap: &FrameTap,
    materialize: &dyn Fn(&GpuPixelBuffer) -> Option<BgraFrame>,
) {
    let mut last_forwarded: Option<Instant> = None;
    let mut retainer = CameraRetainer::default();
    // Real time source for the gate (tests use run_pump-style injection on
    // the single pump; composite timing is covered via overlay unit tests
    // plus the hardware probe).
    let clock = Instant::now;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let (interval, target) = match live.lock() {
            Ok(profile) => (profile.frame_duration(), (profile.w, profile.h)),
            Err(_) => (BRIDGE_TICK, (1280, 720)),
        };
        // Drain the webcam to its latest frame (non-blocking). The survivor
        // is retained across iterations: one camera frame overlays every
        // accepted screen frame until the camera explicitly ends/fails
        // (which clears it). Timeouts keep the previous frame; undecodable
        // GPU packets keep it too.
        if !retainer.dead() {
            loop {
                match camera.next_frame(Duration::ZERO) {
                    Ok(CapturePacket::Cpu(frame)) => retainer.push_cpu(frame),
                    Ok(CapturePacket::Gpu(gpu)) => retainer.push_gpu(&gpu, materialize),
                    Err(NextError::Timeout) => break,
                    Err(NextError::Ended) | Err(NextError::Failed(_)) => {
                        retainer.note_end();
                        break;
                    }
                }
            }
        }
        match screen.next_frame(BRIDGE_TICK) {
            Ok(CapturePacket::Cpu(bgra)) => {
                let now = clock();
                if !should_forward(last_forwarded, now, interval) {
                    continue;
                }
                let (tw, th) = normalize_dims(bgra.w, bgra.h, target.0, target.1);
                let composed = match retainer.frame() {
                    Some(cam) => overlay_pip(&bgra, cam),
                    None => bgra,
                };
                let small = prepare_bgra(composed, tw, th);
                match golive_platform::bgra_to_i420(&small) {
                    Ok(planar) => {
                        let mut data = Vec::with_capacity((planar.w * planar.h * 3 / 2) as usize);
                        data.extend_from_slice(&planar.y);
                        data.extend_from_slice(&planar.u);
                        data.extend_from_slice(&planar.v);
                        let frame = I420Frame {
                            w: planar.w as usize,
                            h: planar.h as usize,
                            data,
                        };
                        last_forwarded = Some(advance_forwarded(last_forwarded, now, interval));
                        // Self-view tap, independent of core backpressure.
                        tap.send(&frame);
                        let _ = core_tx.try_send(ExternalFrame::Cpu(frame));
                    }
                    Err(e) => {
                        eprintln!("screen convert skipped: {e}");
                    }
                }
            }
            Ok(CapturePacket::Gpu(gpu)) => {
                let now = clock();
                if !should_forward(last_forwarded, now, interval) {
                    continue;
                }
                last_forwarded = Some(advance_forwarded(last_forwarded, now, interval));
                // Byte-consumers (PiP overlay, self-view tap) need a CPU
                // readback; without either the packet rides retained. A
                // failed readback degrades to retained — the screen never
                // dies for the overlay.
                if route_gpu_frame(retainer.frame().is_some(), tap.has_viewer())
                    == GpuRoute::Materialize
                {
                    if let Some(bgra) = materialize(&gpu) {
                        let (tw, th) = normalize_dims(bgra.w, bgra.h, target.0, target.1);
                        let composed = match retainer.frame() {
                            Some(cam) => overlay_pip(&bgra, cam),
                            None => bgra,
                        };
                        let small = prepare_bgra(composed, tw, th);
                        match golive_platform::bgra_to_i420(&small) {
                            Ok(planar) => {
                                let frame = pack_i420(planar);
                                tap.send(&frame);
                                let _ = core_tx.try_send(ExternalFrame::Cpu(frame));
                                continue;
                            }
                            Err(e) => {
                                eprintln!("screen convert skipped: {e}");
                            }
                        }
                    }
                }
                let _ = core_tx.try_send(ExternalFrame::Gpu(gpu));
            }
            Err(NextError::Timeout) => continue,
            Err(NextError::Ended) | Err(NextError::Failed(_)) => break,
        }
    }
}

/// Per-frame routing for a retained GPU screen packet (pure): consumers that
/// need bytes — the PiP overlay, the self-view tap — require one CPU
/// materialization; with neither attached the packet rides retained
/// (zero-copy preserved all the way to the encoder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuRoute {
    /// No byte-consumer attached: forward retained, zero CPU copy.
    ForwardRetained,
    /// Overlay and/or tap need pixels: one locked readback, then compose.
    Materialize,
}

pub fn route_gpu_frame(has_overlay: bool, tap_attached: bool) -> GpuRoute {
    if has_overlay || tap_attached {
        GpuRoute::Materialize
    } else {
        GpuRoute::ForwardRetained
    }
}

/// One locked CPU readback of a retained GPU buffer (macOS only; elsewhere
/// always `None`). `None` means the caller degrades — forward retained, skip
/// the tap — never a failure.
fn materialize_gpu(gpu: &GpuPixelBuffer) -> Option<BgraFrame> {
    #[cfg(target_os = "macos")]
    {
        golive_platform_macos::materialize_gpu_buffer(gpu)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = gpu;
        None
    }
}

/// Packs a planar conversion into the core/tap frame shape.
fn pack_i420(planar: golive_platform::PlanarYuv) -> I420Frame {
    let mut data = Vec::with_capacity((planar.w * planar.h * 3 / 2) as usize);
    data.extend_from_slice(&planar.y);
    data.extend_from_slice(&planar.u);
    data.extend_from_slice(&planar.v);
    I420Frame {
        w: planar.w as usize,
        h: planar.h as usize,
        data,
    }
}

/// Feeds the self-view tap from a retained GPU packet: one CPU readback
/// converted at target size. A missing viewer skips everything (zero-copy
/// untouched); a failed readback or convert skips the tap only — the share
/// still flows retained. Never blocks, never fails the share.
fn tap_gpu_frame(
    tap: &FrameTap,
    gpu: &GpuPixelBuffer,
    materialize: &dyn Fn(&GpuPixelBuffer) -> Option<BgraFrame>,
    target: (u32, u32),
) {
    if !tap.has_viewer() {
        return;
    }
    let Some(bgra) = materialize(gpu) else {
        return;
    };
    let (tw, th) = normalize_dims(bgra.w, bgra.h, target.0, target.1);
    let small = prepare_bgra(bgra, tw, th);
    if let Ok(planar) = golive_platform::bgra_to_i420(&small) {
        tap.send(&pack_i420(planar));
    }
}

/// Overlays `overlay` (webcam) scaled onto the bottom-right corner of `base`
/// (screen). Opaque blit, 1/32-of-width margin. Degenerate or unfittable
/// inputs return `base` untouched — the screen never dies for the overlay.
/// Honors `base` stride (padding preserved). Pure.
pub fn overlay_pip(base: &BgraFrame, overlay: &BgraFrame) -> BgraFrame {
    if base.data.is_empty()
        || base.w < 8
        || base.h < 8
        || overlay.data.is_empty()
        || overlay.w == 0
        || overlay.h == 0
    {
        return base.clone();
    }
    let margin = (base.w / 32).max(4).min(32);
    let mut pip_w = (base.w / 4).clamp(48, 480);
    let mut pip_h =
        ((overlay.h as u64 * pip_w as u64 / overlay.w.max(1) as u64) as u32).clamp(2, u32::MAX);
    // Fit inside the base with margin on both axes (aspect preserved).
    if pip_w + margin * 2 > base.w {
        pip_w = base.w.saturating_sub(margin * 2).max(2);
        pip_h = (overlay.h as u64 * pip_w as u64 / overlay.w.max(1) as u64) as u32;
    }
    if pip_h + margin * 2 > base.h {
        pip_h = base.h.saturating_sub(margin * 2).max(2);
        pip_w = (overlay.w as u64 * pip_h as u64 / overlay.h.max(1) as u64) as u32;
    }
    if pip_w < 2 || pip_h < 2 || pip_w + margin * 2 > base.w || pip_h + margin * 2 > base.h {
        return base.clone();
    }
    let small = scale_bgra_bilinear(overlay, pip_w, pip_h);
    if small.data.len() != (pip_w as usize) * (pip_h as usize) * 4 {
        return base.clone();
    }
    let mut out = base.clone();
    let x0 = (base.w - margin - pip_w) as usize;
    let y0 = (base.h - margin - pip_h) as usize;
    for y in 0..pip_h as usize {
        let dst_row = (y0 + y) * base.stride + x0 * 4;
        let src_row = y * small.stride;
        if dst_row + pip_w as usize * 4 > out.data.len()
            || src_row + pip_w as usize * 4 > small.data.len()
        {
            break;
        }
        out.data[dst_row..dst_row + pip_w as usize * 4]
            .copy_from_slice(&small.data[src_row..src_row + pip_w as usize * 4]);
    }
    out
}

// ---------------------------------------------------------------------------
// Local live preview (share modal). Own lightweight OS reads, independent
// from share bridges: cameras pump continuously, screens poll one-shot
// stills (no second OS stream, no GPU-packets problem). Frames are packed
// small (≤320px) as GLP2/format-0 RGBA — the SAME wire the player speaks
// (see player.rs dispatch + parsePlayerFrame), so the frontend reuses its
// renderer. Runs only while the modal is open AND the app is focused
// (frontend-gated); stops are explicit (modal close, blur, share confirm,
// leave). Tauri-free: the command layer forwards packets to a Channel.
// ---------------------------------------------------------------------------

/// Preview long side (px). Small enough to stay cheap over IPC.
pub const PREVIEW_LIVE_MAX_W: u32 = 320;
/// Camera preview cadence (~8fps): live feel without encode-grade cost.
pub const PREVIEW_CAMERA_TICK: Duration = Duration::from_millis(120);
/// Screen still cadence (~1.4fps): cheap + feedback-safe at small size.
pub const PREVIEW_STILL_INTERVAL: Duration = Duration::from_millis(700);
/// Consecutive unreadable stills before the preview reports failure.
pub const PREVIEW_MAX_STILL_ERRORS: u32 = 8;
/// Latest-only preview channel depth (drop, never queue stale).
const PREVIEW_CHANNEL_DEPTH: usize = 2;

/// One preview frame: sequence + dims + tight RGBA pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewPacket {
    pub seq: u32,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

impl PreviewPacket {
    /// GLP2 wire bytes (magic + LE seq/w/h/format + pixels). Must match
    /// player.rs dispatch and the frontend parsePlayerFrame — the three
    /// ship together. `format` is always 0 (RGBA).
    pub fn glp2_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(20 + self.rgba.len());
        bytes.extend_from_slice(b"GLP2");
        bytes.extend_from_slice(&self.seq.to_le_bytes());
        bytes.extend_from_slice(&self.w.to_le_bytes());
        bytes.extend_from_slice(&self.h.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&self.rgba);
        bytes
    }
}

/// Handle to a running preview. `stop()` is idempotent and bounded; `Drop`
/// stops best-effort.
pub struct PreviewHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(test)]
pub(crate) fn preview_handle_for_test(stop: Arc<AtomicBool>) -> PreviewHandle {
    let worker_stop = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        while !worker_stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    PreviewHandle {
        stop,
        thread: Some(thread),
    }
}

impl PreviewHandle {
    pub fn stop(&mut self) -> Result<(), PlatformError> {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = thread.join();
                let _ = done_tx.send(());
            });
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| PlatformError::Internal("stop da prévia excedeu o deadline".into()))?;
        }
        Ok(())
    }
}

impl Drop for PreviewHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Fit `(w, h)` into [`PREVIEW_LIVE_MAX_W`] preserving aspect, even dims,
/// floor 2. Pure. Oversized inputs shrink; small inputs stay native (never
/// upscale: a 160px webcam stays 160px).
pub fn fit_preview_dims(w: u32, h: u32) -> Option<(u32, u32)> {
    if w < 2 || h < 2 {
        return None;
    }
    if w <= PREVIEW_LIVE_MAX_W {
        return Some((w - w % 2, h - h % 2));
    }
    let tw = PREVIEW_LIVE_MAX_W;
    let th = ((h as u64 * tw as u64 / w as u64) as u32).max(2);
    Some((tw - tw % 2, th - th % 2))
}

/// Downscales `frame` to preview size and swizzles BGRA→opaque RGBA.
/// `None` on degenerate input — the caller skips the frame. Pure.
pub fn pack_preview(seq: u32, frame: &BgraFrame) -> Option<PreviewPacket> {
    let (tw, th) = fit_preview_dims(frame.w, frame.h)?;
    let small = scale_bgra_bilinear(frame, tw, th);
    if small.data.len() != (tw as usize) * (th as usize) * 4 {
        return None;
    }
    let mut rgba = Vec::with_capacity(small.data.len());
    for px in small.data.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    Some(PreviewPacket {
        seq,
        w: tw,
        h: th,
        rgba,
    })
}

/// Starts a local preview for one listed source id. Returns the packet
/// channel plus the owning handle. Pre-flight safe: on `Err` nothing runs.
/// Cameras pump their OS stream (startup rendezvous: typed open errors such
/// as denial or a busy device, never a token plus a silent stall); screens
/// poll one-shot stills, pre-flighted by one synchronous grab for the same
/// reason. A pump that ends later (device failure, stop) drops its channel
/// side — the forwarder's stream end is the terminal signal, never silence.
pub fn start_preview_stream(
    kind: SourceKind,
    id: &str,
) -> Result<(mpsc::Receiver<PreviewPacket>, PreviewHandle), PlatformError> {
    start_preview_stream_with_cancel(kind, id, Arc::new(AtomicBool::new(false)))
}

pub fn start_preview_stream_with_cancel(
    kind: SourceKind,
    id: &str,
    cancel: Arc<AtomicBool>,
) -> Result<(mpsc::Receiver<PreviewPacket>, PreviewHandle), PlatformError> {
    if !matches!(
        kind,
        SourceKind::Display | SourceKind::Window | SourceKind::Camera
    ) {
        return Err(PlatformError::InvalidSource {
            reason: "preview: tela, janela ou webcam",
        });
    }
    let id = id.trim();
    if id.is_empty() {
        return Err(PlatformError::InvalidSource { reason: "id vazio" });
    }
    let info = selected_source_info(kind, id)?;
    if matches!(info.kind, SourceKind::Camera) {
        start_camera_preview_with_cancel(info, cancel)
    } else {
        start_still_preview_with_grab(info, cancel, thumbnail_for)
    }
}

/// Screen preview with an injectable still source (production passes
/// [`thumbnail_for`]; tests script frames/failures). One synchronous grab
/// runs BEFORE the token exists: a denied/busy/gone source fails typed here
/// instead of handing out a token that stalls silently and dies ~6 s later
/// on [`PREVIEW_MAX_STILL_ERRORS`].
fn start_still_preview_with_grab(
    info: SourceInfo,
    cancel: Arc<AtomicBool>,
    grab: impl Fn(&SourceInfo) -> Result<BgraFrame, PlatformError> + Send + 'static,
) -> Result<(mpsc::Receiver<PreviewPacket>, PreviewHandle), PlatformError> {
    grab(&info)?;
    let (tx, rx) = mpsc::sync_channel::<PreviewPacket>(PREVIEW_CHANNEL_DEPTH);
    let stop = cancel;
    let stop_ = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("golive-preview".into())
        .spawn(move || {
            pump_still_preview(&info, &tx, &stop_, &mut || grab(&info));
        })
        .map_err(|e| PlatformError::Internal(format!("thread de preview: {e}")))?;
    Ok((
        rx,
        PreviewHandle {
            stop,
            thread: Some(thread),
        },
    ))
}

/// Camera preview with startup rendezvous: the worker reports its open
/// result (typed: denial, busy device, gone source) and the caller waits
/// bounded (auth budget + first frame); any failure or cancel tears the
/// pending worker down instead of returning a token for a stream that will
/// never deliver. The pending worker is never joined here (a native
/// start/stop may block): it is detached with `stop` set, and the
/// same-device lease stays held until that worker really exits — a retry in
/// the window fails typed (busy), never races a live session. Timeout never
/// means released.
fn start_camera_preview_with_cancel(
    info: SourceInfo,
    cancel: Arc<AtomicBool>,
) -> Result<(mpsc::Receiver<PreviewPacket>, PreviewHandle), PlatformError> {
    let (tx, rx) = mpsc::sync_channel::<PreviewPacket>(PREVIEW_CHANNEL_DEPTH);
    let stop = cancel;
    let stop_ = Arc::clone(&stop);
    let (startup_tx, startup_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("golive-preview".into())
        .spawn(move || {
            pump_camera_preview(&info, &tx, Arc::clone(&stop_), startup_tx);
        })
        .map_err(|e| PlatformError::Internal(format!("thread de preview: {e}")))?;
    let deadline = Instant::now() + Duration::from_secs(70);
    loop {
        if stop.load(Ordering::Acquire) {
            drop(thread);
            return Err(PlatformError::Internal("captura cancelada".into()));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            stop.store(true, Ordering::Release);
            drop(thread);
            return Err(PlatformError::Internal(
                "tempo para iniciar a webcam excedido".into(),
            ));
        }
        match startup_rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(Ok(())) => break,
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                drop(thread);
                return Err(error);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(PlatformError::Internal(
                    "a prévia da webcam encerrou".into(),
                ));
            }
        }
    }
    Ok((
        rx,
        PreviewHandle {
            stop,
            thread: Some(thread),
        },
    ))
}

/// Latest-only preview delivery (pure decision): a full channel drops the
/// newest packet and the pump keeps going (the modal always shows fresh
/// frames); only a gone receiver ends the pump. Never blocks, never fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewDelivery {
    Sent,
    DroppedFull,
    ReceiverGone,
}

fn preview_deliver(tx: &mpsc::SyncSender<PreviewPacket>, packet: PreviewPacket) -> PreviewDelivery {
    match tx.try_send(packet) {
        Ok(()) => PreviewDelivery::Sent,
        Err(mpsc::TrySendError::Full(_)) => PreviewDelivery::DroppedFull,
        Err(mpsc::TrySendError::Disconnected(_)) => PreviewDelivery::ReceiverGone,
    }
}

/// How a preview pump ended. The forwarder treats every variant as terminal
/// (the stream end IS the frontend notification). Delivery-stopped (the
/// modal went away) is distinct from teardown-completed (the device
/// ended/failed or an explicit stop ran): only the latter means the OS side
/// is done; the former leaves the worker holding the device lease until its
/// bounded stop lands.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PreviewEnd {
    /// Explicit stop observed.
    Stopped,
    /// The modal stopped draining: upstream keeps the device until stop.
    ReceiverGone,
    /// The device ended cleanly; teardown completed.
    DeviceEnded,
    /// The device failed (or stills failed persistently); teardown completed.
    DeviceFailed,
}

/// Camera preview pump: opens the OS stream once, packs kept frames at
/// preview size on the tick gate, latest-only. Ends on stop, device end,
/// or persistent failure (typed via channel close — the command layer
/// reports the stall; kinds/counts only, never pixels).
fn pump_camera_preview(
    info: &SourceInfo,
    tx: &mpsc::SyncSender<PreviewPacket>,
    stop: Arc<AtomicBool>,
    startup: mpsc::Sender<Result<(), PlatformError>>,
) -> PreviewEnd {
    let config = golive_platform::capture_config_for(640, 480, 10);
    let mut stream = match open_stream(info, &config, Arc::clone(&stop)) {
        Ok((stream, _)) => stream,
        Err(error) => {
            let _ = startup.send(Err(error));
            return PreviewEnd::DeviceFailed;
        }
    };
    let _ = startup.send(Ok(()));
    let mut seq = 0u32;
    let mut last_sent: Option<Instant> = None;
    let end = loop {
        if stop.load(Ordering::Acquire) {
            break PreviewEnd::Stopped;
        }
        match stream.next_frame(PREVIEW_CAMERA_TICK + Duration::from_millis(400)) {
            Ok(CapturePacket::Cpu(bgra)) => {
                let now = Instant::now();
                if !should_forward(last_sent, now, PREVIEW_CAMERA_TICK) {
                    continue;
                }
                if let Some(packet) = pack_preview(seq, &bgra) {
                    seq = seq.wrapping_add(1);
                    last_sent = Some(now);
                    // Latest-only: Full drops the newest (the pump stays on
                    // cadence); only a gone receiver ends the preview.
                    if preview_deliver(tx, packet) == PreviewDelivery::ReceiverGone {
                        break PreviewEnd::ReceiverGone;
                    }
                }
            }
            // Overlay-incompatible GPU packets never occur here (no
            // compositing), but a retained packet has no CPU pixels to
            // pack — skip rather than misinterpret.
            Ok(CapturePacket::Gpu(_)) => continue,
            Err(NextError::Timeout) => continue,
            Err(NextError::Ended) => break PreviewEnd::DeviceEnded,
            Err(NextError::Failed(_)) => break PreviewEnd::DeviceFailed,
        }
    };
    // Bounded teardown: the same-device lease releases only when this
    // returns (the caller never joins us under a lock or on the UI path).
    stream.stop(Duration::from_secs(2)).ok();
    end
}

/// Screen preview pump: one-shot stills on an interval (no persistent OS
/// stream, no GPU packets by construction). `still` is a closure so tests
/// script frames/failures without a backend. Aborts after
/// [`PREVIEW_MAX_STILL_ERRORS`] consecutive failures.
fn pump_still_preview(
    info: &SourceInfo,
    tx: &mpsc::SyncSender<PreviewPacket>,
    stop: &AtomicBool,
    still: &mut dyn FnMut() -> Result<BgraFrame, PlatformError>,
) -> PreviewEnd {
    let _ = info;
    let mut seq = 0u32;
    let mut errors = 0u32;
    loop {
        if stop.load(Ordering::Acquire) {
            return PreviewEnd::Stopped;
        }
        match still() {
            Ok(frame) => {
                errors = 0;
                if let Some(packet) = pack_preview(seq, &frame) {
                    seq = seq.wrapping_add(1);
                    // Latest-only: Full drops the newest (stills keep polling);
                    // only a gone receiver ends the preview.
                    if preview_deliver(tx, packet) == PreviewDelivery::ReceiverGone {
                        return PreviewEnd::ReceiverGone;
                    }
                }
            }
            Err(_) => {
                errors += 1;
                if errors >= PREVIEW_MAX_STILL_ERRORS {
                    return PreviewEnd::DeviceFailed;
                }
            }
        }
        // Interval sleep, stop-responsive (10 slices).
        for _ in 0..10 {
            if stop.load(Ordering::Acquire) {
                return PreviewEnd::Stopped;
            }
            std::thread::sleep(PREVIEW_STILL_INTERVAL / 10);
        }
    }
}

/// Opens + starts the platform stream for one listed source, alongside the
/// backend's [`RestartOrder`] for that source (each arm answers through the
/// platform abstraction — this stays the single cfg-gated selection point;
/// `reconfigure` only ever reads the stored order, never re-queries).
fn open_stream(
    info: &SourceInfo,
    config: &CaptureConfig,
    cancel: Arc<AtomicBool>,
) -> Result<(FrameStream, RestartOrder), PlatformError> {
    #[cfg(target_os = "macos")]
    {
        let mut source = golive_platform_macos::ScSource::open(info).map_err(|e| {
            // open() is validation-only; surface as-is (typed upstream).
            e
        })?;
        let stream = source.start_with_cancel(config, cancel)?;
        Ok((stream, golive_platform_macos::ScSource::restart_order(info)))
    }
    #[cfg(target_os = "windows")]
    {
        let mut source = golive_platform_windows::WindowsSource::open(info)?;
        let stream = source.start_with_cancel(config, cancel)?;
        Ok((
            stream,
            golive_platform_windows::WindowsSource::restart_order(info),
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (info, config, cancel);
        Err(PlatformError::UnsupportedPlatform {
            reason: "captura de tela: apenas macOS (Windows planejado)",
        })
    }
}

/// Time gate: forwards a frame only when at least `interval` elapsed since
/// the last scheduled tick. Pure (the bridge owns the clock).
pub fn should_forward(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

fn advance_forwarded(last: Option<Instant>, now: Instant, interval: Duration) -> Instant {
    match last {
        None => now,
        Some(last) => {
            last + Duration::from_nanos(golive_platform::cadence::advance_capture_clock(
                0,
                now.duration_since(last).as_nanos() as u64,
                interval.as_nanos() as u64,
            ))
        }
    }
}

/// Nearest-neighbor BGRA downscale (or copy at equal size). Honors stride;
/// output is always tight (`stride == dw*4`). Pure.
pub fn scale_bgra_nearest(src: &BgraFrame, dw: u32, dh: u32) -> BgraFrame {
    if src.w == 0 || src.h == 0 || dw == 0 || dh == 0 || src.data.is_empty() {
        return BgraFrame {
            w: dw,
            h: dh,
            stride: (dw as usize) * 4,
            format: PixelFormat::Bgra8888,
            data: Vec::new(),
        };
    }
    let mut data = vec![0u8; (dw as usize) * (dh as usize) * 4];
    for y in 0..dh as usize {
        let sy = (y * src.h as usize / dh as usize).min(src.h as usize - 1);
        for x in 0..dw as usize {
            let sx = (x * src.w as usize / dw as usize).min(src.w as usize - 1);
            let s = sy * src.stride + sx * 4;
            let d = (y * dw as usize + x) * 4;
            if s + 4 <= src.data.len() {
                data[d..d + 4].copy_from_slice(&src.data[s..s + 4]);
            }
        }
    }
    BgraFrame {
        w: dw,
        h: dh,
        stride: (dw as usize) * 4,
        format: PixelFormat::Bgra8888,
        data,
    }
}

/// Bilinear BGRA downscale (honors stride). Same empty/zero contract as nearest.
pub fn scale_bgra_bilinear(src: &BgraFrame, dw: u32, dh: u32) -> BgraFrame {
    if src.w == 0 || src.h == 0 || dw == 0 || dh == 0 || src.data.is_empty() {
        return BgraFrame {
            w: dw,
            h: dh,
            stride: (dw as usize) * 4,
            format: PixelFormat::Bgra8888,
            data: Vec::new(),
        };
    }
    let (sw, sh, dw_us, dh_us) = (src.w as usize, src.h as usize, dw as usize, dh as usize);
    let mut data = vec![0u8; dw_us * dh_us * 4];
    let (sw_f, sh_f, dw_f, dh_f) = (src.w as f32, src.h as f32, dw as f32, dh as f32);
    for y in 0..dh_us {
        let sy = (y as f32 + 0.5) * sh_f / dh_f - 0.5;
        let y0 = (sy.floor() as i64).clamp(0, src.h as i64 - 1) as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let fy = (sy - y0 as f32).clamp(0.0, 1.0);
        for x in 0..dw_us {
            let sx = (x as f32 + 0.5) * sw_f / dw_f - 0.5;
            let x0 = (sx.floor() as i64).clamp(0, src.w as i64 - 1) as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let fx = (sx - x0 as f32).clamp(0.0, 1.0);
            let d = (y * dw_us + x) * 4;
            for c in 0..4 {
                let sample = |px: usize, py: usize| -> f32 {
                    let s = py * src.stride + px * 4 + c;
                    if s < src.data.len() {
                        src.data[s] as f32
                    } else {
                        0.0
                    }
                };
                let top = sample(x0, y0) + (sample(x1, y0) - sample(x0, y0)) * fx;
                let bot = sample(x0, y1) + (sample(x1, y1) - sample(x0, y1)) * fx;
                data[d + c] = (top + (bot - top) * fy).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    BgraFrame {
        w: dw,
        h: dh,
        stride: dw_us * 4,
        format: PixelFormat::Bgra8888,
        data,
    }
}

/// Keep tight, already-sized captures owned across the bridge handoff.
fn prepare_bgra(src: BgraFrame, w: u32, h: u32) -> BgraFrame {
    if (src.w, src.h) == (w, h)
        && w > 0
        && h > 0
        && src.stride == w as usize * 4
        && src.data.len() == src.stride * h as usize
        && src.format == PixelFormat::Bgra8888
    {
        src
    } else {
        scale_bgra_bilinear(&src, w, h)
    }
}

/// Bridge pump: platform packets → throttled core frames, latest-only.
/// Surplus packets die on arrival (no conversion, no alloc); kept CPU
/// frames convert at TARGET size; kept GPU packets forward retained
/// (zero-copy submit downstream) with one CPU readback for the self-view
/// tap only when a viewer is attached (otherwise zero CPU copy anywhere).
/// Ends when the stream ends/fails or `stop` fires; dropping our sender is
/// what tells the core encode loop (disconnect path).
fn pump_bridge(
    stream: &mut FrameStream,
    core_tx: &mpsc::SyncSender<ExternalFrame>,
    stop: &AtomicBool,
    live: &Arc<Mutex<QualityProfile>>,
    mut clock: impl FnMut() -> Instant,
    tap: &FrameTap,
    materialize: &dyn Fn(&GpuPixelBuffer) -> Option<BgraFrame>,
) {
    let mut last_forwarded: Option<Instant> = None;
    let mut trace = Trace::new(Stage::Capture);
    let mut input_trace = Trace::new(Stage::CaptureInput);
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        // One cheap lock per tick: current fps clamp + target dims.
        let (interval, target) = match live.lock() {
            Ok(profile) => (profile.frame_duration(), (profile.w, profile.h)),
            Err(_) => (BRIDGE_TICK, (1280, 720)),
        };
        let packet = stream.next_frame(BRIDGE_TICK);
        if input_trace.start().is_some() {
            if let Some(counts) = stream.take_capture_counts() {
                input_trace.record(
                    TraceSample {
                        frames: counts.received,
                        gate_dropped: counts.gate_dropped,
                        queue_dropped: counts.queue_dropped,
                        invalid_frames: counts.invalid,
                        idle_frames: counts.idle,
                        blank_frames: counts.blank,
                        max_gap_us: counts.max_gap_us,
                        target_fps: (1.0 / interval.as_secs_f64()).round() as u32,
                        ..Default::default()
                    },
                    None,
                );
            }
        }
        match packet {
            Ok(CapturePacket::Gpu(gpu)) => {
                let started = trace.start();
                let now = clock();
                if !should_forward(last_forwarded, now, interval) {
                    trace.record(
                        TraceSample {
                            dropped: 1,
                            ..Default::default()
                        },
                        started,
                    );
                    continue; // over profile fps: drop retained, no pixels
                }
                last_forwarded = Some(advance_forwarded(last_forwarded, now, interval));
                // Self-view tap from a retained packet: one readback when a
                // viewer is attached (no-op otherwise — zero-copy untouched).
                // Independent of core backpressure, like the CPU path.
                tap_gpu_frame(tap, &gpu, materialize, target);
                // Latest-only: a full channel means the core is behind;
                // drop (releasing) rather than queue stale.
                let sent = core_tx.try_send(ExternalFrame::Gpu(gpu)).is_ok();
                trace.record(
                    TraceSample {
                        frames: sent as u64,
                        dropped: (!sent) as u64,
                        gpu_frames: sent as u64,
                        target_fps: (1.0 / interval.as_secs_f64()).round() as u32,
                        ..Default::default()
                    },
                    started,
                );
            }
            Ok(CapturePacket::Cpu(bgra)) => {
                let started = trace.start();
                let now = clock();
                if !should_forward(last_forwarded, now, interval) {
                    trace.record(
                        TraceSample {
                            dropped: 1,
                            ..Default::default()
                        },
                        started,
                    );
                    continue; // over profile fps: drop before touching pixels
                }
                // Fit the capture into the profile (never upscale), so the
                // encoder never sees anything above the contract.
                let (tw, th) = normalize_dims(bgra.w, bgra.h, target.0, target.1);
                let small = prepare_bgra(bgra, tw, th);
                match golive_platform::bgra_to_i420(&small) {
                    Ok(planar) => {
                        let mut data = Vec::with_capacity((planar.w * planar.h * 3 / 2) as usize);
                        data.extend_from_slice(&planar.y);
                        data.extend_from_slice(&planar.u);
                        data.extend_from_slice(&planar.v);
                        let frame = I420Frame {
                            w: planar.w as usize,
                            h: planar.h as usize,
                            data,
                        };
                        last_forwarded = Some(advance_forwarded(last_forwarded, now, interval));
                        // Stage self-view tap: mirrors every converted frame,
                        // independent of core backpressure (a slow encoder
                        // must not stall the local tile).
                        tap.send(&frame);
                        // Latest-only: a full channel means the core is
                        // behind; drop this one rather than queue stale.
                        let sent = core_tx.try_send(ExternalFrame::Cpu(frame)).is_ok();
                        trace.record(
                            TraceSample {
                                frames: sent as u64,
                                dropped: (!sent) as u64,
                                width: tw as u32,
                                height: th as u32,
                                target_fps: (1.0 / interval.as_secs_f64()).round() as u32,
                                ..Default::default()
                            },
                            started,
                        );
                    }
                    Err(e) => {
                        trace.record(
                            TraceSample {
                                errors: 1,
                                ..Default::default()
                            },
                            started,
                        );
                        // Malformed frame: skip one, keep the stream (log the
                        // kind only — never pixels).
                        eprintln!("screen convert skipped: {e}");
                    }
                }
            }
            Err(NextError::Timeout) => {
                trace.record(
                    TraceSample {
                        timeouts: 1,
                        ..Default::default()
                    },
                    None,
                );
                continue;
            }
            Err(NextError::Ended) | Err(NextError::Failed(_)) => break,
        }
    }
    stream.stop(Duration::from_secs(2)).ok();
}

/// Lists capture sources for the UI. Empty/error surfaces typed (the UI
/// shows the reason instead of failing mute).
pub fn enumerate_sources() -> Result<Vec<SourceInfo>, PlatformError> {
    #[cfg(target_os = "macos")]
    {
        golive_platform_macos::enumerate()
    }
    #[cfg(target_os = "windows")]
    {
        golive_platform_windows::enumerate()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err(PlatformError::UnsupportedPlatform {
            reason: "captura de tela: apenas macOS (Windows planejado)",
        })
    }
}

/// Lists one permission domain without touching unrelated platform APIs.
pub fn enumerate_sources_kind(kind: SourceKind) -> Result<Vec<SourceInfo>, PlatformError> {
    #[cfg(target_os = "macos")]
    {
        golive_platform_macos::ScSource::enumerate_kind(kind)
    }
    #[cfg(target_os = "windows")]
    {
        golive_platform_windows::WindowsSource::enumerate_kind(kind)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = kind;
        Err(PlatformError::UnsupportedPlatform {
            reason: "captura de tela: apenas macOS (Windows planejado)",
        })
    }
}

/// Long side of share-modal thumbnails (PNG data URL, ~256px).
pub const PREVIEW_LONG_SIDE: u32 = 256;
/// Aspect fallback for windows (enumerate lists them 0x0 pre-stream).
/// Pixels are always real — this only sets the uniform downscale factor.
const PREVIEW_FALLBACK_DIMS: (u32, u32) = (1920, 1080);

/// One-shot thumbnail for the share modal: PNG data URL + its dims.
#[derive(Clone, Debug)]
pub struct PreviewImage {
    pub data_url: String,
    pub w: u32,
    pub h: u32,
}

/// Lazy one-shot preview for one listed source. Grabs by kind:id without
/// re-enumerating the whole desktop (the Windows tab used to stall the
/// UI by doing a full SCK round-trip per thumbnail). Caller-driven
/// (modal pulls), never polled. Never logs titles/pixels.
pub fn preview_source(kind: SourceKind, id: &str) -> Result<PreviewImage, PlatformError> {
    let info = SourceInfo {
        kind,
        id: id.to_owned(),
        name: String::new(),
        w: 0,
        h: 0,
    };
    let frame = thumbnail_for(&info)?;
    if frame.w == 0 || frame.h == 0 || frame.data.is_empty() {
        return Err(PlatformError::Internal("thumbnail vazio".into()));
    }
    encode_preview(&frame, frame.w as u32, frame.h as u32)
}

/// Platform grab for one listed source (single synchronous still).
fn thumbnail_for(info: &SourceInfo) -> Result<BgraFrame, PlatformError> {
    #[cfg(target_os = "macos")]
    {
        golive_platform_macos::thumbnail(info.kind, &info.id)
    }
    #[cfg(target_os = "windows")]
    {
        golive_platform_windows::thumbnail(info.kind, &info.id)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = info;
        Err(PlatformError::UnsupportedPlatform {
            reason: "miniaturas: apenas macOS (Windows planejado)",
        })
    }
}

/// Scales `frame` by the uniform factor fitting source dims `(sw, sh)` —
/// fallback for 0x0 windows — into [`PREVIEW_LONG_SIDE`]: never upscales,
/// never stretches. Then BGRA→RGBA + PNG encode as a data URL. Pure.
fn encode_preview(frame: &BgraFrame, sw: u32, sh: u32) -> Result<PreviewImage, PlatformError> {
    let (sw, sh) = match (sw, sh) {
        (0, _) | (_, 0) => PREVIEW_FALLBACK_DIMS,
        dims => dims,
    };
    let long = sw.max(sh).max(1) as u64;
    // Uniform factor, capped at 1 (shrink-only); final clamp keeps every
    // thumb within PREVIEW_LONG_SIDE even if pixels outran the listing.
    let num = (PREVIEW_LONG_SIDE as u64).min(long);
    let tw = ((frame.w as u64 * num / long)
        .max(1)
        .min(PREVIEW_LONG_SIDE as u64)) as u32;
    let th = ((frame.h as u64 * num / long)
        .max(1)
        .min(PREVIEW_LONG_SIDE as u64)) as u32;
    let small = scale_bgra_nearest(frame, tw, th);
    if small.data.is_empty() {
        return Err(PlatformError::Internal("thumbnail vazio".into()));
    }
    let mut rgba = Vec::with_capacity(small.data.len());
    for px in small.data.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, small.w, small.h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .and_then(|mut writer| writer.write_image_data(&rgba))
            .map_err(|_| PlatformError::Internal("thumbnail encode falhou".into()))?;
    }
    use base64::Engine as _;
    let data_url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png_bytes)
    );
    Ok(PreviewImage {
        data_url,
        w: small.w,
        h: small.h,
    })
}

/// Re-exported types + helpers for Tauri commands (single import site).
pub use golive_platform::capabilities;
pub use golive_platform::CapabilitySet as Capabilities;
pub use golive_platform::SourceInfo as ListedSource;

#[cfg(test)]
mod tests {
    use super::*;
    use golive_platform::mock::{drive_lifecycle, MockSource};
    use golive_platform::{BgraFrame, PixelFormat, SourceKind};

    fn solid_bgra(w: u32, h: u32, r: u8, g: u8, b: u8) -> BgraFrame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        BgraFrame {
            w,
            h,
            stride: (w * 4) as usize,
            format: PixelFormat::Bgra8888,
            data,
        }
    }

    /// Records hook calls for the restart-ordering tests below.
    struct OrderLog {
        events: std::sync::Mutex<Vec<&'static str>>,
    }

    impl OrderLog {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn stop_old(&self) {
            self.events.lock().expect("log lock").push("stop-old");
        }

        fn spawn_ok(&self) -> Result<(), PlatformError> {
            self.events.lock().expect("log lock").push("spawn-new");
            Ok(())
        }

        fn spawn_err(&self) -> Result<(), PlatformError> {
            self.events.lock().expect("log lock").push("spawn-new");
            Err(PlatformError::Internal("new spawn failed".into()))
        }

        fn events(&self) -> Vec<&'static str> {
            self.events.lock().expect("log lock").clone()
        }
    }

    #[test]
    fn restart_new_first_spawns_before_stopping_old() {
        // macOS SCK tolerates concurrent streams: the replacement is live
        // before the old one retires (glitch-free), and a spawn failure
        // never touches the old stream.
        let log = OrderLog::new();
        let out = restart(RestartOrder::NewFirst, || log.stop_old(), || log.spawn_ok());
        assert!(out.is_ok());
        assert_eq!(log.events(), vec!["spawn-new", "stop-old"]);
        // Spawn failure: old stream untouched (no stop recorded).
        let log = OrderLog::new();
        let out = restart(
            RestartOrder::NewFirst,
            || log.stop_old(),
            || log.spawn_err(),
        );
        assert!(out.is_err());
        assert_eq!(log.events(), vec!["spawn-new"]);
    }

    #[test]
    fn restart_stop_first_stops_old_before_spawning_new() {
        // Windows DXGI allows one duplication per process per output: the
        // old stream must be gone before DuplicateOutput runs again. A spawn
        // failure then propagates with no stream left (loud rollback, never
        // silent resurrection).
        let log = OrderLog::new();
        let out = restart(
            RestartOrder::StopFirst,
            || log.stop_old(),
            || log.spawn_ok(),
        );
        assert!(out.is_ok());
        assert_eq!(log.events(), vec!["stop-old", "spawn-new"]);
        // Spawn failure: old already stopped, error surfaces typed.
        let log = OrderLog::new();
        let out: Result<(), PlatformError> = restart(
            RestartOrder::StopFirst,
            || log.stop_old(),
            || log.spawn_err(),
        );
        assert!(out.is_err());
        assert_eq!(log.events(), vec!["stop-old", "spawn-new"]);
    }

    #[test]
    fn bridge_converts_mock_frames_end_to_end() {
        // Full path with zero OS: mock source → pump → core frames.
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "mock-1".into(),
            name: "Mock".into(),
            w: 64,
            h: 64,
        };
        let mut source = MockSource::open(&info).unwrap();
        source.frames = vec![MockSource::display("x", 64, 64)
            .with_solid_frame(200, 30, 30)
            .frames
            .remove(0)];
        let frames = drive_lifecycle(source, 2).unwrap();
        assert_eq!(frames.len(), 2);
        // Same conversion the bridge applies, on the same bytes.
        let planar = golive_platform::bgra_to_i420(&frames[0]).unwrap();
        assert_eq!((planar.w, planar.h), (64, 64));
        let mean_y: f64 =
            planar.y.iter().map(|b| *b as u64).sum::<u64>() as f64 / planar.y.len() as f64;
        // (200,30,30): Y = 16+(47*200+157*30+16*30+128)>>8 = 73.
        assert!((mean_y - 73.0).abs() <= 5.0, "reddish luma {mean_y}");
    }

    #[test]
    fn bridge_preserves_capture_fps_with_callback_jitter() {
        let interval = QualityProfile::medium().frame_duration();
        let start = Instant::now();
        let (tx, rx) = mpsc::channel();
        for _ in 0..300 {
            tx.send(CapturePacket::Cpu(solid_bgra(2, 2, 40, 80, 120)))
                .unwrap();
        }
        drop(tx);
        let stop = Arc::new(AtomicBool::new(false));
        let mut stream = FrameStream::new(
            rx,
            Arc::new(Mutex::new(None)),
            Arc::clone(&stop),
            std::thread::spawn(|| {}),
        );
        let (core_tx, core_rx) = mpsc::sync_channel(300);
        let live = Arc::new(Mutex::new(QualityProfile::medium()));
        let mut n = 0;
        pump_bridge(
            &mut stream,
            &core_tx,
            &stop,
            &live,
            || {
                let at = start + interval * n + Duration::from_millis((n % 2) as u64);
                n += 1;
                at
            },
            &FrameTap::default(),
            &materialize_gpu,
        );
        let forwarded = core_rx.try_iter().count();
        assert_eq!(forwarded, 300, "30fps capture lost frames to 1ms jitter");
    }

    #[test]
    fn bridge_drops_stale_not_new() {
        // try_send semantics the pump relies on: a full cap-2 channel keeps
        // flowing (drops), never blocks the capture thread.
        let (tx, rx) = mpsc::sync_channel::<u8>(2);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert!(tx.try_send(3).is_err());
        assert_eq!(rx.try_recv().unwrap(), 1);
    }

    #[test]
    fn enumerate_fails_typed_off_macos() {
        // On macOS/Windows enumerate() touches the OS (prompt/denial) —
        // unit tests must NEVER treat that as UnsupportedPlatform.
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let error = enumerate_sources().unwrap_err();
            assert!(matches!(error, PlatformError::UnsupportedPlatform { .. }));
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let caps = crate::screen::capabilities();
            assert!(caps.display.supported);
        }
    }

    #[test]
    fn solid_helper_builds_contract_frames() {
        let _ = solid_bgra(4, 4, 0, 0, 0);
    }

    #[test]
    fn scale_bgra_downsamples_top_left_of_each_cell() {
        // 4x4 gradient in R; 2x2 takes (0,0),(2,0),(0,2),(2,2).
        let mut data = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                let i = (y * 4 + x) * 4;
                data[i] = (y * 10) as u8; // B encodes row
                data[i + 2] = (x * 10) as u8; // R encodes col
                data[i + 3] = 255;
            }
        }
        let src = BgraFrame {
            w: 4,
            h: 4,
            stride: 16,
            format: PixelFormat::Bgra8888,
            data,
        };
        let small = scale_bgra_nearest(&src, 2, 2);
        assert_eq!((small.w, small.h, small.stride), (2, 2, 8));
        // (0,0) stays, (1,0) is old (2,0): R=20.
        assert_eq!(small.data[2], 0);
        assert_eq!(small.data[6], 20);
        // (0,1) is old (0,2): B=20.
        assert_eq!(small.data[8], 20);
        // Degenerate input never panics.
        assert!(scale_bgra_nearest(&src, 0, 2).data.is_empty());
    }

    #[test]
    fn gpu_route_covers_overlay_and_tap_combos() {
        use GpuRoute::*;
        // No byte-consumer: retained, zero CPU copy anywhere.
        assert_eq!(route_gpu_frame(false, false), ForwardRetained);
        // PiP overlay needs bytes (composite), tap needs bytes (self-view).
        assert_eq!(route_gpu_frame(true, false), Materialize);
        assert_eq!(route_gpu_frame(false, true), Materialize);
        assert_eq!(route_gpu_frame(true, true), Materialize);
    }

    /// Scripted stream: preloaded packets, sender kept alive (no `Ended`),
    /// stopped via the flag after the assertions.
    fn scripted_stream() -> (FrameStream, mpsc::Sender<CapturePacket>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel::<CapturePacket>();
        let stop = Arc::new(AtomicBool::new(false));
        let stream = FrameStream::new(
            rx,
            Arc::new(Mutex::new(None)),
            Arc::clone(&stop),
            std::thread::spawn(|| {}),
        );
        (stream, tx, stop)
    }

    /// Fake retained buffer: never dereferenced by test materializers (which
    /// return scripted frames); the release only proves ownership moved once.
    fn fake_gpu() -> GpuPixelBuffer {
        // SAFETY: test-only handle; scripted materializers never touch the
        // pointer, and Drop releases exactly once via the counter below.
        unsafe extern "C-unwind" fn release(ptr: *mut std::ffi::c_void) {
            assert!(!ptr.is_null());
        }
        unsafe { GpuPixelBuffer::from_raw(0x3000 as *mut std::ffi::c_void, 64, 64, 256, release) }
    }

    fn mean_luma(frame: &I420Frame) -> f64 {
        let y = &frame.data[..frame.w * frame.h];
        y.iter().map(|b| *b as u64).sum::<u64>() as f64 / y.len() as f64
    }

    #[test]
    fn composite_gpu_screen_materializes_for_pip_and_tap() {
        // GPU screen + CPU webcam with tap attached: one readback → composed
        // Cpu output (PiP present) + tap fed. Fake readback returns BLACK
        // screen pixels; the white webcam overlay must raise mean luma well
        // above a pure-black conversion.
        let (mut screen_stream, screen_tx, _screen_stop) = scripted_stream();
        let (mut cam_stream, _cam_tx, _cam_stop) = scripted_stream();
        screen_tx.send(CapturePacket::Gpu(fake_gpu())).unwrap();
        _cam_tx
            .send(CapturePacket::Cpu(solid_bgra(16, 16, 255, 255, 255)))
            .unwrap();
        // Screen sender dropped (one scripted packet, then Ended); the camera
        // sender stays alive — a dropped camera sender reads as device death
        // (Ended clears retention), while a live-but-quiet camera retains.
        drop(screen_tx);
        let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(4);
        let tap = FrameTap::default();
        let tap_rx = tap.attach();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_ = Arc::clone(&stop);
        let live = Arc::new(Mutex::new(QualityProfile::medium()));
        let black = solid_bgra(64, 64, 0, 0, 0);
        let worker = std::thread::spawn(move || {
            pump_composite(
                &mut screen_stream,
                &mut cam_stream,
                &core_tx,
                &stop_,
                &live,
                &tap,
                &|_| Some(black.clone()),
            );
        });
        let output = match core_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("composed output")
        {
            ExternalFrame::Cpu(frame) => frame,
            ExternalFrame::Gpu(_) => panic!("overlay/tap need bytes: must materialize, not retain"),
        };
        assert_eq!((output.w, output.h), (64, 64));
        let tap_frame = tap_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("tap fed");
        assert_eq!((tap_frame.w, tap_frame.h), (64, 64));
        // Pure-black conversion baseline vs composed (white PiP corner).
        let baseline = golive_platform::bgra_to_i420(&solid_bgra(64, 64, 0, 0, 0)).unwrap();
        let baseline_mean = mean_luma(&pack_i420(baseline));
        assert!(
            mean_luma(&output) - baseline_mean > 8.0,
            "PiP must survive the GPU readback path"
        );
        stop.store(true, Ordering::Release);
        worker.join().expect("composite exits on stop");
    }

    #[test]
    fn composite_gpu_screen_without_consumers_rides_retained() {
        // GPU screen, no webcam, no tap: zero-copy retained forward and the
        // materializer is never even consulted.
        let (mut screen_stream, screen_tx, _screen_stop) = scripted_stream();
        let (mut cam_stream, _cam_tx, _cam_stop) = scripted_stream();
        screen_tx.send(CapturePacket::Gpu(fake_gpu())).unwrap();
        drop(screen_tx);
        let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(4);
        let tap = FrameTap::default();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_ = Arc::clone(&stop);
        let live = Arc::new(Mutex::new(QualityProfile::medium()));
        let worker = std::thread::spawn(move || {
            pump_composite(
                &mut screen_stream,
                &mut cam_stream,
                &core_tx,
                &stop_,
                &live,
                &tap,
                &|_| panic!("no consumer: must not read back"),
            );
        });
        match core_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("retained output")
        {
            ExternalFrame::Gpu(forwarded) => {
                assert_eq!((forwarded.w, forwarded.h, forwarded.stride), (64, 64, 256));
            }
            ExternalFrame::Cpu(_) => panic!("nothing needs bytes: must stay zero-copy"),
        }
        stop.store(true, Ordering::Release);
        worker.join().expect("composite exits on stop");
    }

    #[test]
    fn bridge_gpu_screen_feeds_tap_keeping_zero_copy_share() {
        // Single share of a GPU screen with self-view attached: the CORE feed
        // stays retained (zero-copy) while the tap gets converted pixels from
        // one readback.
        let (mut stream, tx, _held) = scripted_stream();
        tx.send(CapturePacket::Gpu(fake_gpu())).unwrap();
        drop(tx);
        let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(4);
        let tap = FrameTap::default();
        let tap_rx = tap.attach();
        let stop = AtomicBool::new(false);
        let live = Arc::new(Mutex::new(QualityProfile::medium()));
        let black = solid_bgra(64, 64, 10, 20, 30);
        pump_bridge(
            &mut stream,
            &core_tx,
            &stop,
            &live,
            Instant::now,
            &tap,
            &|_| Some(black.clone()),
        );
        match core_rx.try_recv().expect("share feed") {
            ExternalFrame::Gpu(forwarded) => {
                assert_eq!((forwarded.w, forwarded.h, forwarded.stride), (64, 64, 256));
            }
            ExternalFrame::Cpu(_) => panic!("share feed must stay retained"),
        }
        let tap_frame = tap_rx.try_recv().expect("tap fed from readback");
        assert_eq!((tap_frame.w, tap_frame.h), (64, 64));
        assert_eq!(tap_frame.data.len(), 64 * 64 * 3 / 2);
    }

    #[test]
    fn camera_retainer_keeps_quiet_camera_clears_only_on_end() {
        let mut retainer = CameraRetainer::default();
        assert!(retainer.frame().is_none());
        // A timeout with no frame yet keeps nothing (still no PiP).
        assert!(!retainer.dead());
        retainer.push_cpu(solid_bgra(16, 16, 255, 255, 255));
        assert!(retainer.frame().is_some());
        // Undecodable GPU packets and timeouts keep the previous frame.
        retainer.push_gpu(&fake_gpu(), &|_| None);
        assert!(retainer.frame().is_some());
        // A decodable GPU packet replaces it.
        let replacement = solid_bgra(8, 8, 1, 2, 3);
        retainer.push_gpu(&fake_gpu(), &|_| Some(replacement.clone()));
        assert_eq!(retainer.frame(), Some(&replacement));
        // Explicit end/failure clears; nothing resurrects after.
        retainer.note_end();
        assert!(retainer.dead());
        assert!(retainer.frame().is_none());
    }

    #[test]
    fn composite_retains_one_camera_frame_across_two_screen_frames() {
        // One webcam frame precedes two accepted screen frames: BOTH outputs
        // carry the overlay (the second must not go bare on a quiet camera).
        // Screen BLACK, webcam WHITE: composed outputs sit well above the
        // pure-black baseline.
        let (mut screen_stream, screen_tx) = {
            let (tx, rx) = mpsc::channel::<CapturePacket>();
            (
                FrameStream::new(
                    rx,
                    Arc::new(Mutex::new(None)),
                    Arc::new(AtomicBool::new(false)),
                    std::thread::spawn(|| {}),
                ),
                tx,
            )
        };
        let (mut cam_stream, cam_tx) = {
            let (tx, rx) = mpsc::channel::<CapturePacket>();
            (
                FrameStream::new(
                    rx,
                    Arc::new(Mutex::new(None)),
                    Arc::new(AtomicBool::new(false)),
                    std::thread::spawn(|| {}),
                ),
                tx,
            )
        };
        cam_tx
            .send(CapturePacket::Cpu(solid_bgra(16, 16, 255, 255, 255)))
            .unwrap();
        let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(4);
        let tap = FrameTap::default();
        let tap_rx = tap.attach();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_ = Arc::clone(&stop);
        // 10 fps profile (100 ms gate): 250 ms-spaced screens both pass.
        let live = Arc::new(Mutex::new(
            QualityProfile::custom(64, 48, 500, 10).expect("10fps test profile"),
        ));
        let worker = std::thread::spawn(move || {
            pump_composite(
                &mut screen_stream,
                &mut cam_stream,
                &core_tx,
                &stop_,
                &live,
                &tap,
                &|_| None,
            );
        });
        let mut outputs = Vec::new();
        for _ in 0..2 {
            screen_tx
                .send(CapturePacket::Cpu(solid_bgra(64, 64, 0, 0, 0)))
                .unwrap();
            match core_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("screen output")
            {
                ExternalFrame::Cpu(frame) => outputs.push(frame),
                ExternalFrame::Gpu(_) => panic!("CPU-only feed emitted GPU"),
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        drop(screen_tx);
        drop(cam_tx);
        stop.store(true, Ordering::Release);
        worker.join().expect("composite exits");
        assert_eq!(outputs.len(), 2);
        let baseline = golive_platform::bgra_to_i420(&solid_bgra(48, 48, 0, 0, 0)).unwrap();
        let baseline_mean = mean_luma(&pack_i420(baseline));
        for (i, output) in outputs.iter().enumerate() {
            assert_eq!((output.w, output.h), (48, 48), "output {i} dims");
            assert!(
                mean_luma(output) - baseline_mean > 8.0,
                "output {i} must carry the retained PiP"
            );
        }
        // The tap mirrored both composed frames too.
        for i in 0..2 {
            tap_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|e| panic!("tap frame {i}: {e:?}"));
        }
        assert!(
            tap_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "tap sees exactly the two composed frames"
        );
    }

    #[test]
    fn overlay_pip_lands_bottom_right_keeping_base_elsewhere() {
        // 64x64 blue screen + 16x16 red webcam: PiP is base.w/4 = 16 wide.
        let base = solid_bgra(64, 64, 0, 0, 255);
        let cam = solid_bgra(16, 16, 255, 0, 0);
        let out = overlay_pip(&base, &cam);
        assert_eq!((out.w, out.h, out.stride), (64, 64, 256));
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * out.stride + x * 4) as usize;
            [
                out.data[i],
                out.data[i + 1],
                out.data[i + 2],
                out.data[i + 3],
            ]
        };
        // Top-left stays screen blue (BGRA: B=255).
        assert_eq!(px(2, 2), [255, 0, 0, 255]);
        // Bottom-right corner is webcam red with margin (64/32=2 → margin 4).
        assert_eq!(px(63 - 4 - 1, 63 - 4 - 1), [0, 0, 255, 255]);
    }

    #[test]
    fn overlay_pip_degrades_to_base_untouched() {
        let base = solid_bgra(64, 64, 0, 0, 255);
        let empty = BgraFrame {
            w: 0,
            h: 0,
            stride: 0,
            format: PixelFormat::Bgra8888,
            data: Vec::new(),
        };
        // Empty overlay, empty base, tiny base: screen never dies.
        assert_eq!(overlay_pip(&base, &empty).data, base.data);
        assert_eq!(overlay_pip(&empty, &base).data, empty.data);
        let tiny = solid_bgra(4, 4, 1, 2, 3);
        assert_eq!(overlay_pip(&tiny, &base).data, tiny.data);
    }

    #[test]
    fn overlay_pip_preserves_stride_padding() {
        // Base with padded stride: overlay must not smear padding rows.
        let data = vec![7u8; 8 * 4 + 16];
        let base = BgraFrame {
            w: 2,
            h: 4,
            stride: 12,
            format: PixelFormat::Bgra8888,
            data,
        };
        let cam = solid_bgra(8, 8, 200, 30, 30);
        let out = overlay_pip(&base, &cam);
        assert_eq!(out.stride, 12);
        assert_eq!(out.data.len(), base.data.len());
    }

    #[test]
    fn preview_fit_shrinks_wide_never_upscales() {
        assert_eq!(fit_preview_dims(0, 10), None);
        assert_eq!(fit_preview_dims(640, 480), Some((320, 240)));
        assert_eq!(fit_preview_dims(1920, 1080), Some((320, 180)));
        // Small native stays native (evened, never upscaled).
        assert_eq!(fit_preview_dims(160, 120), Some((160, 120)));
        assert_eq!(fit_preview_dims(161, 121), Some((160, 120)));
    }

    #[test]
    fn preview_pack_swizzles_bgra_to_opaque_rgba() {
        // 2x2: red, green, blue, white in BGRA → RGBA with forced alpha.
        let src = BgraFrame {
            w: 2,
            h: 2,
            stride: 8,
            format: PixelFormat::Bgra8888,
            data: vec![0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0],
        };
        let packet = pack_preview(7, &src).unwrap();
        assert_eq!((packet.seq, packet.w, packet.h), (7, 2, 2));
        assert_eq!(
            packet.rgba,
            vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,]
        );
        // GLP2 wire: magic + LE seq/w/h/format + pixels (player-compatible).
        let wire = packet.glp2_bytes();
        assert_eq!(&wire[..4], b"GLP2");
        assert_eq!(&wire[4..8], &7u32.to_le_bytes());
        assert_eq!(&wire[8..12], &2u32.to_le_bytes());
        assert_eq!(&wire[12..16], &2u32.to_le_bytes());
        assert_eq!(&wire[16..20], &0u32.to_le_bytes());
        assert_eq!(&wire[20..], packet.rgba.as_slice());
        // Degenerate input never panics — the pump skips the frame.
        let empty = BgraFrame {
            w: 0,
            h: 0,
            stride: 0,
            format: PixelFormat::Bgra8888,
            data: Vec::new(),
        };
        assert!(pack_preview(0, &empty).is_none());
    }

    #[test]
    fn still_pump_streams_scripted_stills_then_stops() {
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "mock-1".into(),
            name: "Mock".into(),
            w: 64,
            h: 64,
        };
        let frame = solid_bgra(64, 64, 10, 200, 30);
        let (tx, rx) = mpsc::sync_channel::<PreviewPacket>(8);
        let stop = AtomicBool::new(false);
        let worker = std::thread::spawn(move || {
            let mut still = || Ok(frame.clone());
            pump_still_preview(&info, &tx, &stop, &mut still)
        });
        let mut got = Vec::new();
        for _ in 0..2 {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(packet) => got.push(packet),
                Err(e) => panic!("still pump stalled: {e:?}"),
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].w, got[0].h), (64, 64));
        assert_eq!(got[1].seq, got[0].seq + 1);
        assert_eq!(got[0].rgba.len(), 64 * 64 * 4);
        drop(rx);
        // Delivery-stopped (modal gone) is its own terminal reason — not a
        // device failure, not a silent stall.
        assert_eq!(
            worker.join().expect("pump exits when receiver drops"),
            PreviewEnd::ReceiverGone
        );
    }

    #[test]
    fn still_pump_reports_explicit_stop() {
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "mock-1".into(),
            name: "Mock".into(),
            w: 64,
            h: 64,
        };
        let (tx, _rx) = mpsc::sync_channel::<PreviewPacket>(8);
        let stop = AtomicBool::new(true);
        let frame = solid_bgra(64, 64, 10, 200, 30);
        assert_eq!(
            pump_still_preview(&info, &tx, &stop, &mut || Ok(frame.clone())),
            PreviewEnd::Stopped
        );
    }

    #[test]
    fn still_pump_aborts_on_persistent_failure() {
        let info = SourceInfo {
            kind: SourceKind::Window,
            id: "gone".into(),
            name: "Mock".into(),
            w: 0,
            h: 0,
        };
        let (tx, rx) = mpsc::sync_channel::<PreviewPacket>(8);
        let stop = AtomicBool::new(false);
        // 8 consecutive failures × 700ms: aborts in ~6s, never hangs.
        let start = Instant::now();
        let end = pump_still_preview(
            &info,
            &tx,
            &stop,
            &mut || -> Result<BgraFrame, PlatformError> {
                Err(PlatformError::SourceGone { id: "gone".into() })
            },
        );
        assert_eq!(end, PreviewEnd::DeviceFailed);
        assert!(start.elapsed() < Duration::from_secs(30));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn still_preview_startup_failure_is_typed_never_a_stalled_token() {
        // A denied/busy/gone source fails at startup with its typed error —
        // no token, no thread, no ~6 s silent stall before the error budget
        // trips.
        let info = SourceInfo {
            kind: SourceKind::Window,
            id: "gone".into(),
            name: "Mock".into(),
            w: 0,
            h: 0,
        };
        let error =
            match start_still_preview_with_grab(info, Arc::new(AtomicBool::new(false)), |_| {
                Err(PlatformError::SourceGone { id: "gone".into() })
            }) {
                Ok(_) => panic!("startup must fail typed"),
                Err(error) => error,
            };
        assert!(
            matches!(error, PlatformError::SourceGone { .. }),
            "unexpected {error:?}"
        );
    }

    #[test]
    fn still_preview_startup_success_streams_scripted_stills() {
        // Pre-flight grab passes → token + live packets from the same source.
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "mock-1".into(),
            name: "Mock".into(),
            w: 64,
            h: 64,
        };
        let frame = solid_bgra(64, 64, 10, 200, 30);
        let (rx, mut handle) =
            start_still_preview_with_grab(info, Arc::new(AtomicBool::new(false)), move |_| {
                Ok(frame.clone())
            })
            .expect("startup passes");
        let first = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("still packet");
        assert_eq!((first.w, first.h), (64, 64));
        assert_eq!(first.rgba.len(), 64 * 64 * 4);
        assert!(handle.stop().is_ok());
    }

    #[test]
    fn preview_delivery_keeps_flowing_on_full_stops_only_on_gone() {
        let packet = || PreviewPacket {
            seq: 1,
            w: 2,
            h: 2,
            rgba: vec![0u8; 16],
        };
        // Empty channel sends.
        let (tx, rx) = mpsc::sync_channel::<PreviewPacket>(1);
        assert_eq!(preview_deliver(&tx, packet()), PreviewDelivery::Sent);
        // Full channel (receiver alive, never drains) drops newest — the pump
        // must NOT treat this as an exit.
        assert_eq!(preview_deliver(&tx, packet()), PreviewDelivery::DroppedFull);
        assert_eq!(preview_deliver(&tx, packet()), PreviewDelivery::DroppedFull);
        // Draining unblocks normal delivery again.
        assert_eq!(rx.try_recv().unwrap().seq, 1);
        assert_eq!(preview_deliver(&tx, packet()), PreviewDelivery::Sent);
        // A gone receiver is the only terminal delivery state.
        drop(rx);
        let (dead_tx, dead_rx) = mpsc::sync_channel::<PreviewPacket>(1);
        drop(dead_rx);
        assert_eq!(
            preview_deliver(&dead_tx, packet()),
            PreviewDelivery::ReceiverGone
        );
    }

    #[test]
    fn still_pump_survives_full_channel_and_delivers_after_drain() {
        // Latest-only contract at the pump level: while the modal stops
        // draining (full channel) the pump keeps polling instead of exiting;
        // once drained, fresh packets flow again.
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "mock-1".into(),
            name: "Mock".into(),
            w: 64,
            h: 64,
        };
        let frame = solid_bgra(64, 64, 10, 200, 30);
        let (tx, rx) = mpsc::sync_channel::<PreviewPacket>(PREVIEW_CHANNEL_DEPTH);
        // Prefill to Full with stale markers (seq far above any live seq).
        for _ in 0..PREVIEW_CHANNEL_DEPTH {
            tx.try_send(PreviewPacket {
                seq: 9999,
                w: 2,
                h: 2,
                rgba: vec![0u8; 16],
            })
            .unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_ = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let mut still = || Ok(frame.clone());
            pump_still_preview(&info, &tx, &stop_, &mut still);
        });
        // The pump must still be alive well after two still intervals: Full
        // no longer kills it.
        std::thread::sleep(PREVIEW_STILL_INTERVAL * 2 + Duration::from_millis(200));
        assert!(!worker.is_finished(), "Full must not end the pump");
        // Drain the stale markers; the next live packet arrives fresh.
        let mut fresh = None;
        for _ in 0..8 {
            match rx.recv_timeout(Duration::from_secs(5)).expect("packet") {
                packet if packet.seq == 9999 => continue,
                packet => {
                    fresh = Some(packet);
                    break;
                }
            }
        }
        let fresh = fresh.expect("live packet after drain");
        assert_eq!((fresh.w, fresh.h), (64, 64));
        assert_eq!(fresh.rgba.len(), 64 * 64 * 4);
        stop.store(true, Ordering::Release);
        worker.join().expect("pump exits on stop");
    }

    #[test]
    fn tap_mirrors_full_skips_dead_clears() {
        let tap = FrameTap::default();
        assert!(!tap.is_attached());
        let frame = |v: u8| I420Frame {
            w: 2,
            h: 2,
            data: vec![v; 6],
        };
        // Unattached send is a no-op (share without self-view costs nothing).
        tap.send(&frame(1));
        let rx = tap.attach();
        assert!(tap.is_attached());
        tap.send(&frame(2));
        let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(got.data, vec![2u8; 6]);
        // Full channel (cap 2) drops newest instead of blocking the pump.
        tap.send(&frame(3));
        tap.send(&frame(4));
        tap.send(&frame(5));
        let _ = rx.try_iter().count();
        // Dead receiver clears the tap on next send.
        drop(rx);
        tap.send(&frame(6));
        assert!(!tap.is_attached());
        // Re-attach replaces cleanly.
        let rx2 = tap.attach();
        assert!(tap.is_attached());
        tap.send(&frame(7));
        assert_eq!(
            rx2.recv_timeout(Duration::from_secs(2)).unwrap().data,
            vec![7u8; 6]
        );
    }

    #[test]
    #[ignore]
    fn hw_bridge_tap_mirrors_shared_camera_frames() {
        // Real webcam through a live share bridge (the exact mechanism the
        // stage self-view uses). Run explicitly, serially (single-open):
        // cargo test -p golive-app --lib hw_bridge_tap -- --ignored --nocapture --test-threads=1
        // Virtual devices with no fulfillable mode are skipped honestly.
        let listed = enumerate_sources().expect("real enumerate");
        let live = Arc::new(Mutex::new(QualityProfile::medium()));
        for cam in listed.iter().filter(|s| s.kind == SourceKind::Camera) {
            let (core_rx, handle, _) = match start_capture_for(
                SourceKind::Camera,
                &cam.id,
                QualityProfile::medium(),
                Arc::clone(&live),
            ) {
                Ok(opened) => opened,
                Err(e) => {
                    eprintln!("tap skipping camera: {e}");
                    continue;
                }
            };
            let tap_rx = handle.attach_tap();
            // The core side is intentionally undrained (cap-2 latest-only):
            // the tap must flow independently of core backpressure.
            drop(core_rx);
            match tap_rx.recv_timeout(Duration::from_secs(15)) {
                Ok(frame) => {
                    assert!(frame.w >= 2 && frame.h >= 2);
                    assert_eq!(frame.data.len(), frame.w * frame.h * 3 / 2);
                    let second = tap_rx
                        .recv_timeout(Duration::from_secs(15))
                        .expect("tap frame");
                    assert_eq!((second.w, second.h), (frame.w, frame.h));
                    return;
                }
                Err(e) => eprintln!("tap camera without frames: {e:?}"),
            }
        }
        panic!("no tap-proven webcam");
    }

    #[test]
    #[ignore]
    fn hw_camera_preview_streams_small_packets() {
        // Needs a physical webcam; never runs in CI:
        // `cargo test -p golive-app --lib hw_camera_preview -- --ignored --nocapture`
        // Virtual devices with no fulfillable mode are skipped honestly.
        let listed = enumerate_sources().expect("real enumerate");
        // A device may open yet never deliver frames (virtual cameras) —
        // only a received packet proves a previewable webcam.
        let mut opened = None;
        for cam in listed.iter().filter(|s| s.kind == SourceKind::Camera) {
            match start_preview_stream(cam.kind, &cam.id) {
                Ok((rx, handle)) => match rx.recv_timeout(Duration::from_secs(12)) {
                    Ok(first) => {
                        opened = Some((first, rx, handle));
                        break;
                    }
                    Err(e) => eprintln!("preview camera without frames: {e}"),
                },
                Err(e) => eprintln!("preview skipping camera: {e}"),
            }
        }
        let (first, rx, mut handle) = opened.expect("a previewable webcam");
        assert!((2..=PREVIEW_LIVE_MAX_W).contains(&first.w));
        assert_eq!(first.rgba.len(), first.w as usize * first.h as usize * 4);
        for _ in 0..2 {
            let packet = rx
                .recv_timeout(Duration::from_secs(15))
                .expect("preview packet");
            assert!((2..=PREVIEW_LIVE_MAX_W).contains(&packet.w));
            assert_eq!(packet.rgba.len(), packet.w as usize * packet.h as usize * 4);
            let wire = packet.glp2_bytes();
            assert_eq!(&wire[..4], b"GLP2");
        }
        let _ = handle.stop();
    }

    #[test]
    fn camera_pack_path_fits_mock_frames_at_preview_size() {
        use golive_platform::mock::MockSource;
        use golive_platform::VideoSource;
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "mock-1".into(),
            name: "Mock".into(),
            w: 640,
            h: 480,
        };
        let mut source = MockSource::open(&info).unwrap();
        source.frames = vec![MockSource::display("x", 640, 480)
            .with_solid_frame(200, 30, 30)
            .frames
            .remove(0)];
        let stream = source.start(&CaptureConfig::default()).unwrap();
        let (tx, rx) = mpsc::sync_channel::<PreviewPacket>(8);
        let pump = std::thread::spawn(move || {
            let mut seq = 0u32;
            for _ in 0..2 {
                match stream.next_frame(Duration::from_secs(5)) {
                    Ok(CapturePacket::Cpu(bgra)) => {
                        if let Some(packet) = pack_preview(seq, &bgra) {
                            seq += 1;
                            let _ = tx.try_send(packet);
                        }
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        });
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // 640x480 capture fits preview 320px untouched in aspect.
        assert_eq!((first.w, first.h), (320, 240));
        assert_eq!(first.rgba.len(), 320 * 240 * 4);
        pump.join().expect("packing pass ends");
    }

    #[test]
    fn scale_bgra_bilinear_identity_and_empty() {
        let src = BgraFrame {
            w: 2,
            h: 2,
            stride: 8,
            format: PixelFormat::Bgra8888,
            data: (0..16).collect(),
        };
        let same = scale_bgra_bilinear(&src, 2, 2);
        assert_eq!(same.stride, 8);
        assert_eq!(same.data.len(), 16);
        assert!(scale_bgra_bilinear(&src, 0, 2).data.is_empty());
    }

    #[test]
    fn preview_encode_downscales_and_emits_png_data_url() {
        use base64::Engine as _;
        // Pure (no OS): 512x256 solid → long side 256, PNG magic after base64.
        let frame = solid_bgra(512, 256, 10, 200, 30);
        let preview = encode_preview(&frame, 512, 256).expect("encodes");
        assert_eq!((preview.w, preview.h), (256, 128));
        let b64 = preview
            .data_url
            .strip_prefix("data:image/png;base64,")
            .expect("data url prefix");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("valid base64");
        assert_eq!(
            &raw[..8],
            &[137, 80, 78, 71, 13, 10, 26, 10],
            "PNG signature"
        );
        // 0x0 source dims (windows pre-stream) use the 16:9 fallback factor:
        // 512x256 px * 256/1920 → 68x34, aspect preserved, no stretch.
        let fallback = encode_preview(&frame, 0, 0).expect("fallback encodes");
        assert_eq!((fallback.w, fallback.h), (68, 34));
        // Never upscales: 64x64 stays native.
        let tiny = encode_preview(&solid_bgra(64, 64, 0, 0, 0), 64, 64).expect("tiny encodes");
        assert_eq!((tiny.w, tiny.h), (64, 64));
        // Degenerate pixels error typed, never panic.
        let empty = BgraFrame {
            w: 0,
            h: 0,
            stride: 0,
            format: PixelFormat::Bgra8888,
            data: Vec::new(),
        };
        assert!(encode_preview(&empty, 64, 64).is_err());
    }

    #[test]
    fn should_forward_gates_on_interval() {
        let now = Instant::now();
        let interval = Duration::from_millis(100);
        assert!(should_forward(None, now, interval));
        assert!(!should_forward(Some(now), now, interval));
        assert!(should_forward(Some(now - interval), now, interval));
        assert!(!should_forward(
            Some(now - interval + Duration::from_millis(1)),
            now,
            interval
        ));
    }

    /// Feeds N frames as fast as possible through the real pump with a
    /// 1 fps profile: exactly the first survives (timestamp gate, no
    /// conversion on drops), at profile dims (downscale before convert).
    /// Returns decoded CPU frames (GPU packets pass through untouched and
    /// are unwrapped by their own test below).
    fn run_pump(frames: Vec<BgraFrame>, profile: QualityProfile) -> Vec<I420Frame> {
        use golive_platform::{CapturePacket, FrameStream};
        let (tx, rx) = mpsc::channel::<CapturePacket>();
        let worker = std::thread::spawn(move || {
            for frame in frames {
                if tx.send(CapturePacket::Cpu(frame)).is_err() {
                    break;
                }
            }
        });
        let mut stream = FrameStream::new(
            rx,
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicBool::new(false)),
            worker,
        );
        let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(2);
        let stop = AtomicBool::new(false);
        let live = Arc::new(Mutex::new(profile));
        pump_bridge(
            &mut stream,
            &core_tx,
            &stop,
            &live,
            Instant::now,
            &FrameTap::default(),
            &materialize_gpu,
        );
        let mut out = Vec::new();
        while let Ok(packet) = core_rx.recv_timeout(Duration::from_millis(200)) {
            match packet {
                ExternalFrame::Cpu(frame) => out.push(frame),
                ExternalFrame::Gpu(_) => panic!("CPU-only feed emitted GPU"),
            }
        }
        out
    }

    #[test]
    fn bridge_clamps_fps_and_downscales_before_convert() {
        let profile = QualityProfile::custom(64, 48, 500, 1).expect("1fps test profile");
        let frames = vec![solid_bgra(128, 96, 200, 30, 30); 10];
        let out = run_pump(frames, profile);
        // 10 back-to-back frames inside one 1 s window: exactly one survives.
        assert_eq!(out.len(), 1, "fps clamp drops surplus before conversion");
        // 128x96 capture fit into 64x48: exact target dims (even, no upscale).
        assert_eq!((out[0].w, out[0].h), (64, 48));
        assert_eq!(out[0].data.len(), 64 * 48 * 3 / 2);
    }

    #[test]
    fn bridge_passes_through_at_native_size() {
        let profile = QualityProfile::medium();
        let out = run_pump(vec![solid_bgra(64, 64, 0, 200, 0)], profile);
        assert_eq!(out.len(), 1);
        // 64x64 into 1280x720: no upscale, native size kept.
        assert_eq!((out[0].w, out[0].h), (64, 64));
    }

    #[test]
    fn bridge_forwards_gpu_retained_without_converting() {
        use golive_platform::{CapturePacket, FrameStream, GpuPixelBuffer};
        use std::sync::atomic::AtomicU64;
        static RELEASES: AtomicU64 = AtomicU64::new(0);
        // SAFETY: test-only release — counts, never dereferences.
        unsafe extern "C-unwind" fn mock_release(ptr: *mut std::ffi::c_void) {
            assert!(!ptr.is_null());
            RELEASES.fetch_add(1, Ordering::SeqCst);
        }
        RELEASES.store(0, Ordering::SeqCst);
        // SAFETY: dangling non-null pointer; nothing touches pixels here,
        // the pump must forward it untouched and Drop releases once.
        let gpu = unsafe {
            GpuPixelBuffer::from_raw(0x2000 as *mut std::ffi::c_void, 128, 96, 512, mock_release)
        };
        let (tx, rx) = mpsc::channel::<CapturePacket>();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(CapturePacket::Gpu(gpu));
        });
        let mut stream = FrameStream::new(
            rx,
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicBool::new(false)),
            worker,
        );
        let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(2);
        let stop = AtomicBool::new(false);
        let live = Arc::new(Mutex::new(QualityProfile::medium()));
        pump_bridge(
            &mut stream,
            &core_tx,
            &stop,
            &live,
            Instant::now,
            &FrameTap::default(),
            &materialize_gpu,
        );
        match core_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("gpu forwarded")
        {
            ExternalFrame::Gpu(forwarded) => {
                // Untouched: same dims/stride, still owned (no convert ran —
                // a conversion would have produced Cpu instead).
                assert_eq!((forwarded.w, forwarded.h, forwarded.stride), (128, 96, 512));
                drop(forwarded);
            }
            ExternalFrame::Cpu(_) => panic!("GPU packet must not convert in the bridge"),
        }
        assert_eq!(RELEASES.load(Ordering::SeqCst), 1, "exactly one release");
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::*;

    #[test]
    fn bridge_keeps_matching_capture_allocation() {
        let src = BgraFrame {
            w: 2,
            h: 2,
            stride: 8,
            format: PixelFormat::Bgra8888,
            data: (0..16).collect(),
        };
        let pixels = src.data.as_ptr();
        let expected = golive_platform::bgra_to_i420(&src).unwrap();
        let ready = prepare_bgra(src, 2, 2);
        assert_eq!(ready.data.as_ptr(), pixels);
        assert_eq!(golive_platform::bgra_to_i420(&ready).unwrap(), expected);
    }

    #[test]
    fn bridge_preserves_padding_and_short_frame_normalization() {
        for len in [0, 9, 20] {
            let src = BgraFrame {
                w: 2,
                h: 2,
                stride: 12,
                format: PixelFormat::Bgra8888,
                data: vec![42; len],
            };
            for dims in [(2, 2), (2, 4)] {
                let expected = scale_bgra_bilinear(&src, dims.0, dims.1);
                let ready = prepare_bgra(src.clone(), dims.0, dims.1);
                assert_eq!(ready.data, expected.data);
                assert_eq!(ready.stride, expected.stride);
            }
        }
    }
}

/// Backend selection remains in the app's existing platform glue.
pub(crate) fn install_decoder_backend() {
    #[cfg(target_os = "macos")]
    {
        golive_core::media::install_decoder_factory(golive_platform_macos::decode::new_decoder);
        golive_core::media::install_media_cpu_clock(golive_platform_macos::decode::thread_cpu_us);
    }
}

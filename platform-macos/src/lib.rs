//! macOS capture backend: ScreenCaptureKit via pure `objc2` bindings.
//!
//! WHY objc2 and not a Swift-bridge crate: Swift bridges need Swift-compat
//! static archives at link time plus a Swift-concurrency dylib that exists
//! nowhere on CLT-only machines — the packaged app would abort at launch.
//! `objc2` links plain system frameworks, which every Mac has. (Verified
//! the hard way; see git history.)
//!
//! Mapping to the pure contract (`golive-platform`):
//! - `enumerate()` → `getShareableContentWithCompletionHandler:` → displays
//!   + on-screen windows (titles shown in UI, never logged).
//! - `open()` validates id/kind only (fast, no OS).
//! - `start()` re-enumerates, matches the id (gone → `SourceGone`), builds
//!   filter + BGRA config + output object, and spawns a supervisor thread
//!   owning every SCK object. First OS contact happens there — this is
//!   where the system permission prompt appears. Startup is
//!   rendezvous-bounded (ready or typed error).
//! - Frames arrive on our own dispatch queue; the output object copies BGRA
//!   bytes (stride-aware) into a bounded latest-only channel.
//! - `stop()` signals the supervisor, which stops capture (indicators
//!   released deterministically) and exits; join is deadline-bounded.
//!
//! Colorspace: SCK delivers display-native BGRA and this backend does NOT
//! pin a color space (an unverifiable string would risk breaking real
//! capture without TCC to test it). [`golive_platform::bgra_to_i420`]
//! treats values as sRGB-encoded BT.709 input — exact on sRGB displays,
//! approximate on Display-P3 panels (exact gamut mapping needs GPU).
//!
//! Mid-stream disappearance (unplugged display, closed window) surfaces via
//! the stream delegate (`didStopWithError:`) into the error slot; silence
//! otherwise means a static screen (the core repeats the last frame).
//!
//! Aggregate counts only in logs (never titles, pixels, or tokens).
//!
//! # Safety
//! All `unsafe` here is mechanical ObjC interop: null-checked pointers,
//! exact-size reads, lock/unlock pairing via RAII guard. The one semantic
//! claim is the CVImageBuffer→CVPixelBuffer cast, justified by SCK's
//! documented screen-capture contract plus a runtime pixel-format check
//! (non-BGRA aborts the frame, never misinterprets it).

mod audio;
mod camera;

pub use audio::{list_audio_apps, start_audio_tap, AudioTap};

use golive_platform::{
    BgraFrame, CaptureConfig, CapturePacket, FrameStream, GpuPixelBuffer, PixelFormat,
    PlatformError, RestartOrder, SourceInfo, SourceKind, VideoSource,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Startup rendezvous bound: SCK setup (incl. first prompt wait) is quick;
/// beyond this the start is declared failed, never hung.
const START_DEADLINE: Duration = Duration::from_secs(8);
/// Channel depth: one in flight, one waiting. Newest drops when full, so
/// memory stays flat and staleness is bounded (~2 frames), never queued.
const CHANNEL_DEPTH: usize = 2;
/// SCK denial codes: -3810 (SCStreamErrorUserDeclined, observed) and -3801
/// (userDeclined — what TCC delivers on macOS 26). Both mean the user said
/// no (or the app isn't listed): honest permission hint, never Internal.
/// Denial also surfaces as hidden content instead of erroring (null
/// shareable content / empty enumerate map separately — see below); this
/// covers the error-carrying paths (shareable fetch, stream start).
const SC_USER_DECLINED: i32 = -3810;
const SC_TCC_DECLINED: i32 = -3801;

/// A macOS display or window selected from [`enumerate`].
#[derive(Clone, Debug)]
pub struct ScSource {
    info: SourceInfo,
}

impl ScSource {
    fn validated(info: &SourceInfo) -> Result<Self, PlatformError> {
        if info.id.trim().is_empty() {
            return Err(PlatformError::InvalidSource { reason: "id vazio" });
        }
        match info.kind {
            SourceKind::Display | SourceKind::Window | SourceKind::Camera => {
                Ok(Self { info: info.clone() })
            }
        }
    }

    /// Current macOS version as `(major, minor)`, read from
    /// `/System/Library/CoreServices/SystemVersion.plist` with std::fs only.
    /// Used for `OsVersionTooOld`.
    fn os_version() -> (u32, u32) {
        let text = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
            .unwrap_or_default();
        parse_product_version(&text).unwrap_or((0, 0))
    }
}

/// Parses `ProductVersion` (e.g. "14.2.1") out of SystemVersion.plist text.
/// Pure and unit-tested; the file read above is the only impure step.
fn parse_product_version(text: &str) -> Option<(u32, u32)> {
    let mut lines = text.lines();
    for line in &mut lines {
        if line.contains("<key>ProductVersion</key>") {
            let value = lines.next().unwrap_or("");
            let version = value
                .trim()
                .trim_start_matches("<string>")
                .trim_end_matches("</string>")
                .trim();
            let mut parts = version.split('.');
            let major: u32 = parts.next()?.parse().ok()?;
            let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            return Some((major, minor));
        }
    }
    None
}

/// NSError → typed error. Only domain + code travel (never message text —
/// system strings stay out of logs by policy).
fn map_ns_error(domain: &str, code: i32) -> PlatformError {
    if domain.contains("ScreenCaptureKit") && (code == SC_USER_DECLINED || code == SC_TCC_DECLINED)
    {
        return PlatformError::permission_denied();
    }
    // -3801 is delivered by TCC itself (outside the SCK domain) on macOS 26:
    // the code is denial-specific, so it maps regardless of domain.
    if code == SC_TCC_DECLINED {
        return PlatformError::permission_denied();
    }
    // Denial also hides content instead of erroring; empty enumerate maps
    // separately (see enumerate). Anything else is internal, redacted to
    // domain + code (+ OS major for triage — not a secret).
    let (major, _) = ScSource::os_version();
    PlatformError::Internal(format!("captura falhou ({domain} #{code}, macOS {major})"))
}

fn ns_error_parts(error: &NSError) -> (String, i32) {
    // Plain getters (safe bindings); only domain + code travel onward.
    (error.domain().to_string(), error.code() as i32)
}

/// Enumerate screen capture targets only. Kept independent from AVFoundation
/// so a camera selection never performs ScreenCaptureKit authorization work.
fn enumerate_screen_sources() -> Result<Vec<SourceInfo>, PlatformError> {
    let content = shareable_content()?;
    let mut out = Vec::new();
    unsafe {
        for display in content.displays().iter() {
            let id = display.displayID();
            let w = display.width().max(0) as u32;
            let h = display.height().max(0) as u32;
            out.push(SourceInfo {
                kind: SourceKind::Display,
                id: id.to_string(),
                name: format!("Display {id} · {w}x{h}"),
                w,
                h,
            });
        }
        for window in content.windows().iter() {
            let title = window
                .title()
                .map(|title| title.to_string())
                .unwrap_or_default();
            let name = if title.trim().is_empty() {
                "Janela sem título".into()
            } else {
                title
            };
            let (w, h) = window_even_size(&window);
            out.push(SourceInfo {
                kind: SourceKind::Window,
                id: window.windowID().to_string(),
                name,
                w,
                h,
            });
        }
    }
    if out.is_empty() {
        return Err(PlatformError::permission_denied());
    }
    Ok(out)
}

/// Enumerate one capture permission domain without touching the other.
pub fn enumerate_kind(kind: SourceKind) -> Result<Vec<SourceInfo>, PlatformError> {
    enumerate_kind_with(
        kind,
        || enumerate_screen_sources(),
        || camera::enumerate_cameras(),
    )
}

fn enumerate_kind_with(
    kind: SourceKind,
    screen: impl FnOnce() -> Result<Vec<SourceInfo>, PlatformError>,
    cameras: impl FnOnce() -> Vec<SourceInfo>,
) -> Result<Vec<SourceInfo>, PlatformError> {
    match kind {
        SourceKind::Camera => Ok(cameras()),
        SourceKind::Display | SourceKind::Window => Ok(screen()?
            .into_iter()
            .filter(|source| source.kind == kind)
            .collect()),
    }
}

/// List all available targets. A screen-permission denial does not hide
/// discoverable webcams; unrelated SCK failures are still surfaced.
pub fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
    enumerate_with(enumerate_screen_sources, camera::enumerate_cameras)
}

fn enumerate_with(
    screen: impl FnOnce() -> Result<Vec<SourceInfo>, PlatformError>,
    cameras: impl FnOnce() -> Vec<SourceInfo>,
) -> Result<Vec<SourceInfo>, PlatformError> {
    let cameras = cameras();
    match screen() {
        Ok(mut screens) => {
            screens.extend(cameras);
            Ok(screens)
        }
        Err(PlatformError::PermissionDenied { .. }) if !cameras.is_empty() => Ok(cameras),
        Err(error) => Err(error),
    }
}

/// Blocking shareable-content fetch with deadline. Errors map typed;
/// null content without error maps to denial (hidden content).
fn shareable_content() -> Result<Retained<SCShareableContent>, PlatformError> {
    let (tx, rx) = mpsc::channel::<Result<Retained<SCShareableContent>, PlatformError>>();
    let block = block2::RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if !error.is_null() {
                // SAFETY: non-null per the null check; borrowed for the call.
                let (domain, code) = unsafe { ns_error_parts(&*error) };
                Err(map_ns_error(&domain, code))
            } else if content.is_null() {
                Err(PlatformError::permission_denied())
            } else {
                // SAFETY: non-null per the check; SCK passes a +0 reference
                // that lives through this call — retain for the channel.
                let retained: Retained<SCShareableContent> =
                    unsafe { Retained::retain(content).unwrap() };
                Ok(retained)
            };
            let _ = tx.send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&block);
    }
    match rx.recv_timeout(START_DEADLINE) {
        Ok(result) => result,
        Err(_) => Err(PlatformError::Internal("timeout ao listar fontes".into())),
    }
}

/// One-shot still for the share-modal preview: a single
/// `CGWindowListCreateImage` grab (synchronous, no stream, no extra OS
/// prompt beyond what enumerate already needed). Displays capture their
/// own bounds; windows capture by id (`CGRectNull` + IncludingWindow).
/// Returns tight BGRA pixels (window-list images are premultiplied-first
/// little-endian; the preview path treats the bytes as BGRA8888 —
/// premultiplication is visually negligible at thumbnail size).
///
/// One call per source, caller-driven (lazy modal pulls) — never polled.
/// Failures are typed (bad id, gone source, empty grab); titles and pixels
/// never reach logs.
#[allow(deprecated)] // deprecated for capture; still the one-shot still API.
pub fn thumbnail(kind: SourceKind, id: &str) -> Result<BgraFrame, PlatformError> {
    // Do not open a camera from a one-shot thumbnail pull: modal thumbnails
    // may be automatic, while AVFoundation permission is gesture-scoped.
    if matches!(kind, SourceKind::Camera) {
        let _ = id;
        return Err(PlatformError::Internal(
            "prévia de webcam requer fluxo explícito".into(),
        ));
    }
    use objc2_core_graphics::{
        kCGNullWindowID, CGDataProvider, CGDisplayBounds, CGImageGetBitsPerComponent,
        CGImageGetBitsPerPixel, CGImageGetBytesPerRow, CGImageGetDataProvider, CGImageGetHeight,
        CGImageGetWidth, CGRectIsNull, CGRectNull, CGWindowImageOption, CGWindowListCreateImage,
        CGWindowListOption,
    };
    let window_id: u32 = id
        .trim()
        .parse()
        .map_err(|_| PlatformError::InvalidSource {
            reason: "id de fonte inválido",
        })?;
    // SAFETY: plain CoreGraphics C calls. Every pointer is null-checked
    // (Option returns), every read is exact-size (dims/stride come from the
    // getters below), and the pixel copy outlives nothing: bytes are copied
    // while the retained CFData is alive.
    unsafe {
        let image = match kind {
            SourceKind::Display => {
                let bounds = CGDisplayBounds(window_id);
                if CGRectIsNull(bounds) {
                    return Err(PlatformError::SourceGone { id: id.to_owned() });
                }
                CGWindowListCreateImage(
                    bounds,
                    CGWindowListOption::OptionOnScreenOnly,
                    kCGNullWindowID,
                    CGWindowImageOption::Default,
                )
            }
            SourceKind::Window => CGWindowListCreateImage(
                CGRectNull,
                CGWindowListOption::OptionIncludingWindow,
                window_id,
                CGWindowImageOption::Default,
            ),
            // Webcams return early above (AVFoundation stills, no CG).
            SourceKind::Camera => unreachable!("webcam sai cedo acima"),
        };
        let image =
            image.ok_or_else(|| PlatformError::Internal("thumbnail indisponível".into()))?;
        let w = CGImageGetWidth(Some(&image));
        let h = CGImageGetHeight(Some(&image));
        let stride = CGImageGetBytesPerRow(Some(&image));
        if w == 0
            || h == 0
            || w > 8192
            || h > 8192
            || CGImageGetBitsPerComponent(Some(&image)) != 8
            || CGImageGetBitsPerPixel(Some(&image)) != 32
            || stride < w.saturating_mul(4)
        {
            return Err(PlatformError::Internal("thumbnail vazio".into()));
        }
        let provider = CGImageGetDataProvider(Some(&image))
            .ok_or_else(|| PlatformError::Internal("thumbnail vazio".into()))?;
        let data = CGDataProvider::data(Some(&provider))
            .ok_or_else(|| PlatformError::Internal("thumbnail vazio".into()))?;
        let len = data.length().max(0) as usize;
        if len < h.saturating_mul(stride) {
            return Err(PlatformError::Internal("thumbnail vazio".into()));
        }
        let bytes = std::slice::from_raw_parts(data.byte_ptr(), len);
        // Tight copy honoring provider stride (row padding dropped).
        let mut pixels = vec![0u8; w * h * 4];
        for (dst_row, src_row) in pixels
            .chunks_exact_mut(w * 4)
            .zip(bytes.chunks(stride))
            .take(h)
        {
            dst_row.copy_from_slice(&src_row[..w * 4]);
        }
        Ok(BgraFrame {
            w: w as u32,
            h: h as u32,
            stride: w * 4,
            format: PixelFormat::Bgra8888,
            data: pixels,
        })
    }
}

impl VideoSource for ScSource {
    fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
        enumerate()
    }

    fn enumerate_kind(kind: SourceKind) -> Result<Vec<SourceInfo>, PlatformError> {
        enumerate_kind(kind)
    }

    fn open(info: &SourceInfo) -> Result<Self, PlatformError> {
        Self::validated(info)
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<FrameStream, PlatformError> {
        self.start_with_cancel(config, Arc::new(AtomicBool::new(false)))
    }

    fn start_with_cancel(
        &mut self,
        config: &CaptureConfig,
        cancel: Arc<AtomicBool>,
    ) -> Result<FrameStream, PlatformError> {
        // Webcams bypass SCK entirely (AVFoundation delegate, owned worker).
        if matches!(self.info.kind, SourceKind::Camera) {
            return camera::start_camera(&self.info, config, cancel);
        }
        if cancel.load(Ordering::Acquire) {
            return Err(PlatformError::Internal("captura cancelada".into()));
        }
        let info = self.info.clone();
        let config = *config;
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), PlatformError>>();
        let (frame_tx, frame_rx) = mpsc::sync_channel::<CapturePacket>(CHANNEL_DEPTH);
        let error: Arc<Mutex<Option<PlatformError>>> = Arc::new(Mutex::new(None));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let error_ = Arc::clone(&error);
        let stop_ = Arc::clone(&stop_flag);
        let probe = Arc::new(golive_platform::capture_probe::CaptureProbe::default());
        let worker_probe = Arc::clone(&probe);
        // Every SCK object lives on this thread: no Send questions, prompt
        // teardown, deterministic startup rendezvous below.
        let worker = std::thread::Builder::new()
            .name("golive-sck".into())
            .spawn(move || {
                run_capture(
                    info,
                    config,
                    frame_tx,
                    error_,
                    stop_,
                    ready_tx,
                    worker_probe,
                );
            })
            .map_err(|e| PlatformError::Internal(format!("thread de captura: {e}")))?;
        match ready_rx.recv_timeout(START_DEADLINE) {
            Ok(Ok(())) => {
                Ok(FrameStream::new(frame_rx, error, stop_flag, worker).with_capture_probe(probe))
            }
            Ok(Err(error)) => {
                stop_flag.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop_flag.store(true, Ordering::Release);
                let _ = worker.join();
                Err(PlatformError::Internal("timeout ao iniciar captura".into()))
            }
        }
    }

    fn restart_order(info: &SourceInfo) -> RestartOrder {
        // Same-device AVFoundation reopen races driver teardown: stop the
        // old stream first. SCK tolerates concurrent streams (NewFirst).
        match info.kind {
            SourceKind::Camera => RestartOrder::StopFirst,
            SourceKind::Display | SourceKind::Window => RestartOrder::NewFirst,
        }
    }
}

// ---------------------------------------------------------------------------
// Output object: SCK frames in, bounded channel out.
// ---------------------------------------------------------------------------

struct OutputIvars {
    probe: Arc<golive_platform::capture_probe::CaptureProbe>,
    tx: Mutex<mpsc::SyncSender<CapturePacket>>,
    // Written here, read by golive-platform's FrameStream::next_frame
    // (cross-crate use the dead-code lint cannot see).
    #[allow(dead_code)]
    error_slot: Arc<Mutex<Option<PlatformError>>>,
    /// Nanos of the last ACCEPTED frame (monotonic process clock below).
    /// Atomic: the SCK queue thread gates on it with zero locks.
    last_ns: AtomicU64,
    /// Minimum spacing between accepted frames (1/profile-fps). Fixed per
    /// stream; a quality change starts a new stream (see reconfigure).
    interval_ns: u64,
}

/// Monotonic nanos without lock or alloc (callback-safe).
fn now_ns() -> u64 {
    use std::sync::OnceLock;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

/// Cadence gate: accept only when `interval_ns` elapsed since the last
/// scheduled tick. Pure + wrapping-sub so a (practically impossible)
/// clock step never wedges the stream open or shut.
pub fn gate_open(last_ns: u64, now_ns: u64, interval_ns: u64) -> bool {
    now_ns.wrapping_sub(last_ns) >= interval_ns
}

/// Initial gate state: pretend the last frame landed exactly one interval
/// ago, so the first real frame is always accepted no matter how fast SCK
/// delivers after setup. Pure (the stream builder stamps `now_ns()` here).
pub fn initial_last_ns(now: u64, interval_ns: u64) -> u64 {
    now.wrapping_sub(interval_ns)
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl
    // (ivars are plain data, released field-wise); SyncSender is Send and
    // guarded access makes cross-thread use sound.
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    struct CaptureOutput;

    unsafe impl NSObjectProtocol for CaptureOutput {}

    unsafe impl SCStreamOutput for CaptureOutput {
        // ObjC selector name is mandatory (not Rust snake case).
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &objc2_core_media::CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Screen {
                return;
            }
            // GATE FIRST: surplus frames die here touching nothing — no
            // lock, no copy, no alloc.
            let ivars = self.ivars();
            let now = now_ns();
            ivars.probe.arrival(now);
            if !gate_open(
                ivars.last_ns.load(Ordering::Relaxed),
                now,
                ivars.interval_ns,
            ) {
                ivars.probe.gate_dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let packet = match retain_gpu_packet(sample_buffer) {
                // Zero-copy fast path: retained IOSurface buffer straight
                // to the channel (VideoToolbox submits it without a CPU
                // copy downstream).
                Some(packet) => packet,
                // Fallback: CPU copy when zero-copy is unavailable
                // (non-BGRA, non-IOSurface, retain failure). Logged once
                // per stream — persistent fallback is a surprise worth one
                // line, per-frame spam is not.
                None => match extract_bgra(sample_buffer) {
                    Some(frame) => {
                        log_fallback_once();
                        CapturePacket::Cpu(frame)
                    }
                    None => {
                        ivars.probe.invalid.fetch_add(1, Ordering::Relaxed);
                        match frame_status::sample_status(sample_buffer) {
                            Some(1) => {
                                ivars.probe.idle.fetch_add(1, Ordering::Relaxed);
                            }
                            Some(2) => {
                                ivars.probe.blank.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => (),
                        }
                        return;
                    }
                },
            };
            if let Ok(tx) = ivars.tx.lock() {
                // Latest-only: drop newest (not oldest) when full.
                if tx.try_send(packet).is_err() {
                    ivars.probe.queue_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Cadence accounts accepted frames even when the channel was
            // full: copies/retains stay capped at profile fps while the
            // core lags (never spins on a slow consumer).
            ivars.last_ns.store(
                golive_platform::cadence::advance_capture_clock(
                    ivars.last_ns.load(Ordering::Relaxed),
                    now,
                    ivars.interval_ns,
                ),
                Ordering::Relaxed,
            );
        }
    }
);

/// Per-frame routing decision (pure, unit-tested with a mock copy
/// counter): gate first, then GPU-retain when the sample is directly
/// submittable (BGRA + IOSurface-backed), else one CPU copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameRoute {
    /// Off cadence: die touching nothing.
    Drop,
    /// Directly submittable: retain, zero CPU copy.
    RetainGpu,
    /// Anything else BGRA: exactly one stride-aware copy.
    CopyCpu,
}

pub fn route_frame(gate_open: bool, is_bgra: bool, iosurface_backed: bool) -> FrameRoute {
    if !gate_open {
        FrameRoute::Drop
    } else if is_bgra && iosurface_backed {
        FrameRoute::RetainGpu
    } else if is_bgra {
        FrameRoute::CopyCpu
    } else {
        FrameRoute::Drop
    }
}

/// First-occurrence fallback log (one line per process, not per frame).
fn log_fallback_once() {
    use std::sync::OnceLock;
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.set(()).is_ok() {
        eprintln!("golive: capture zero-copy unavailable, CPU-copy fallback");
    }
}

/// Retain the sample's pixel buffer for zero-copy submit downstream.
/// `None` when the sample is not directly submittable (non-BGRA,
/// non-IOSurface, or a failed retain) — the caller falls back to the CPU
/// copy path. Never touches a pixel.
fn retain_gpu_packet(sample: &objc2_core_media::CMSampleBuffer) -> Option<CapturePacket> {
    // SAFETY: SCK screen output with a BGRA configuration always carries a
    // CVPixelBuffer; format + backing validated before retaining.
    unsafe {
        let image = sample.image_buffer()?;
        // Toll-free: CVPixelBuffer IS-A CVImageBuffer.
        let pixel = &*(image.as_ref() as *const CVImageBuffer as *const CVPixelBuffer);
        if CVPixelBufferGetPixelFormatType(pixel) != kCVPixelFormatType_32BGRA {
            return None;
        }
        // VT submits any CVPixelBuffer, but the zero-copy fast path is
        // only worthy of the name on IOSurface backing (no CPU residency
        // surprises); anything else takes the copy path.
        if CVPixelBufferGetIOSurface(Some(pixel)).is_none() {
            return None;
        }
        let w = CVPixelBufferGetWidth(pixel);
        let h = CVPixelBufferGetHeight(pixel);
        let stride = CVPixelBufferGetBytesPerRow(pixel);
        if w == 0 || h == 0 || w > 8192 || h > 8192 || stride < w * 4 {
            return None;
        }
        let retained = Retained::retain(pixel as *const CVPixelBuffer as *mut CVPixelBuffer)?;
        let raw = Retained::into_raw(retained);
        // SAFETY: +1 from the retain above, balanced by the release fn.
        let gpu = GpuPixelBuffer::from_raw(
            raw as *mut c_void,
            w as u32,
            h as u32,
            stride,
            release_cv_pixel_buffer,
        );
        Some(CapturePacket::Gpu(gpu))
    }
}

/// Balances one handler-side retain (see `retain_gpu_packet`).
///
/// # Safety
/// `ptr` must be a live +1 CVPixelBuffer (or null, which is a no-op).
unsafe extern "C-unwind" fn release_cv_pixel_buffer(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let Some(retained) = Retained::<CVPixelBuffer>::from_raw(ptr as *mut CVPixelBuffer) else {
            return;
        };
        drop(retained);
    }
}

/// Copies one BGRA sample into an owned frame. Returns None (drop frame)
/// on any anomaly — a dropped frame beats a misinterpreted one.
///
/// This is the FALLBACK path (see `retain_gpu_packet`): it runs only when
/// zero-copy is unavailable (non-BGRA, non-IOSurface, failed retain). The
/// copy stays profile-sized and cadence-capped, so even the fallback is a
/// fraction of the old always-copy-everything behavior.
fn extract_bgra(sample: &objc2_core_media::CMSampleBuffer) -> Option<BgraFrame> {
    // SAFETY: SCK screen output with a BGRA configuration always carries a
    // CVPixelBuffer; the pixel-format check below validates before touching.
    unsafe {
        let image = sample.image_buffer()?;
        // Toll-free: CVPixelBuffer IS-A CVImageBuffer. Validated by format
        // below before touching a byte.
        let pixel = &*(image.as_ref() as *const CVImageBuffer as *const CVPixelBuffer);
        copy_locked_cv_pixel_buffer(pixel)
    }
}

/// Materializes a retained [`GpuPixelBuffer`] into owned CPU pixels for the
/// consumers that cannot use a retained buffer: the screen+webcam compositor
/// (PiP overlay needs bytes) and the stage self-view tap (needs I420).
/// Returns `None` (caller degrades: forward retained, skip tap) on any
/// anomaly — a degraded frame beats a misinterpreted one. Shares the exact
/// stride-aware locked copy below with the SCK fallback path; zero-copy
/// forwarding elsewhere is untouched.
pub fn materialize_gpu_buffer(gpu: &GpuPixelBuffer) -> Option<BgraFrame> {
    let ptr = gpu.as_ptr();
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` is the live +1 CVPixelBuffer the capture backend retained
    // (see `retain_gpu_packet`); borrowed read-only here, never stored, and
    // the lock/unlock pairing lives inside the helper.
    unsafe { copy_locked_cv_pixel_buffer(&*(ptr as *const CVPixelBuffer)) }
}

fn copy_locked_cv_pixel_buffer(pixel: &CVPixelBuffer) -> Option<BgraFrame> {
    // SAFETY: plain CoreVideo C calls on a borrowed pixel buffer. Format and
    // dims validated before touching; lock/unlock paired; the slice is
    // exact-size and never outlives the lock.
    unsafe {
        if CVPixelBufferGetPixelFormatType(pixel) != kCVPixelFormatType_32BGRA {
            return None;
        }
        let w = CVPixelBufferGetWidth(pixel);
        let h = CVPixelBufferGetHeight(pixel);
        let stride = CVPixelBufferGetBytesPerRow(pixel);
        if w == 0 || h == 0 || w > 8192 || h > 8192 || stride < w * 4 {
            return None;
        }
        if CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(pixel) as *const u8;
        let frame = if base.is_null() {
            None
        } else {
            let bytes = stride
                .checked_mul(h.saturating_sub(1))?
                .checked_add(w.checked_mul(4)?)?;
            let src = std::slice::from_raw_parts(base, bytes);
            let mut data = vec![0u8; bytes];
            for (dst_row, src_row) in data
                .chunks_exact_mut(stride)
                .zip(src.chunks(stride))
                .take(h)
            {
                dst_row[..w * 4].copy_from_slice(&src_row[..w * 4]);
            }
            Some(BgraFrame {
                w: w as u32,
                h: h as u32,
                stride,
                format: PixelFormat::Bgra8888,
                data,
            })
        };
        CVPixelBufferUnlockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly);
        frame
    }
}

/// Supervisor body: enumerate → match id → filter → config → stream → pump
/// until stop. Reports readiness once; SCK teardown always runs on exit.
#[allow(clippy::too_many_lines)]
fn window_even_size(window: &SCWindow) -> (u32, u32) {
    let frame = unsafe { window.frame() };
    let w = (frame.size.width.round().max(0.0) as u32).max(2) & !1;
    let h = (frame.size.height.round().max(0.0) as u32).max(2) & !1;
    (w.max(2), h.max(2))
}

fn fit_capture_size(native_w: u32, native_h: u32, config: &CaptureConfig) -> CaptureConfig {
    let scale = (config.width as f64 / native_w.max(1) as f64)
        .min(config.height as f64 / native_h.max(1) as f64)
        .min(1.0);
    let w = ((native_w as f64 * scale) as u32).max(2) & !1;
    let h = ((native_h as f64 * scale) as u32).max(2) & !1;
    golive_platform::capture_config_for(w, h, config.fps)
}

fn run_capture(
    info: SourceInfo,
    config: CaptureConfig,
    frame_tx: mpsc::SyncSender<CapturePacket>,
    error_slot: Arc<Mutex<Option<PlatformError>>>,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<(), PlatformError>>,
    probe: Arc<golive_platform::capture_probe::CaptureProbe>,
) {
    let fail = |error: PlatformError| {
        if let Ok(mut guard) = error_slot.lock() {
            *guard = Some(error.clone());
        }
        let _ = ready_tx.send(Err(error));
    };
    let content = match shareable_content() {
        Ok(content) => content,
        Err(error) => {
            fail(error);
            return;
        }
    };
    // SAFETY: all objects below are created and consumed on this thread;
    // callbacks only move owned data (channels, flags).
    unsafe {
        let mut tracked_window: Option<Retained<SCWindow>> = None;
        let filter: Option<Retained<SCContentFilter>> = match info.kind {
            SourceKind::Display => {
                let id: u32 = match info.id.parse() {
                    Ok(id) => id,
                    Err(_) => {
                        fail(PlatformError::InvalidSource {
                            reason: "id de display inválido",
                        });
                        return;
                    }
                };
                match content.displays().iter().find(|d| d.displayID() == id) {
                    Some(display) => {
                        let empty: Retained<NSArray<SCWindow>> = NSArray::from_slice(&[]);
                        Some(SCContentFilter::initWithDisplay_excludingWindows(
                            SCContentFilter::alloc(),
                            &display,
                            &empty,
                        ))
                    }
                    None => {
                        fail(PlatformError::SourceGone {
                            id: info.id.clone(),
                        });
                        return;
                    }
                }
            }
            SourceKind::Window => {
                let id: u32 = match info.id.parse() {
                    Ok(id) => id,
                    Err(_) => {
                        fail(PlatformError::InvalidSource {
                            reason: "id de janela inválido",
                        });
                        return;
                    }
                };
                match content.windows().iter().find(|w| w.windowID() == id) {
                    Some(window) => {
                        tracked_window = Some(window.clone());
                        Some(SCContentFilter::initWithDesktopIndependentWindow(
                            SCContentFilter::alloc(),
                            &window,
                        ))
                    }
                    None => {
                        fail(PlatformError::SourceGone {
                            id: info.id.clone(),
                        });
                        return;
                    }
                }
            }
            // Unreachable via start() (webcams take start_camera), kept as a
            // loud guard so a future caller can never SCK-capture a camera.
            SourceKind::Camera => {
                fail(PlatformError::Internal(
                    "webcam usa o caminho AVFoundation".into(),
                ));
                return;
            }
        };
        let Some(filter) = filter else {
            fail(PlatformError::Internal("filtro vazio".into()));
            return;
        };
        let stream_config = SCStreamConfiguration::new();
        let applied = if let Some(window) = tracked_window.as_ref() {
            let (nw, nh) = window_even_size(window);
            fit_capture_size(nw, nh, &config)
        } else {
            golive_platform::capture_config_for(config.width, config.height, config.fps)
        };
        stream_config.setWidth(applied.width as usize);
        stream_config.setHeight(applied.height as usize);
        stream_config.setPixelFormat(kCVPixelFormatType_32BGRA);
        stream_config.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: applied.fps as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        stream_config.setShowsCursor(true);
        // Redacted proof the config reached the stream: dims + fps + kind
        // only (never ids, names, pixels).
        let kind = match info.kind {
            SourceKind::Display => "display",
            SourceKind::Window => "window",
            SourceKind::Camera => "camera",
        };
        eprintln!(
            "golive: capture {}x{}@{}fps ({kind})",
            applied.width, applied.height, applied.fps
        );
        let interval_ns = 1_000_000_000u64 / applied.fps.max(1) as u64;
        let output = CaptureOutput::alloc().set_ivars(OutputIvars {
            probe,
            tx: Mutex::new(frame_tx),
            error_slot: Arc::clone(&error_slot),
            last_ns: AtomicU64::new(initial_last_ns(now_ns(), interval_ns)),
            interval_ns,
        });
        // SAFETY: standard init after alloc with ivars set.
        let output: Retained<CaptureOutput> = msg_send![super(output), init];
        let output_proto = ProtocolObject::<dyn SCStreamOutput>::from_retained(output);
        let stream: Retained<SCStream> = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &stream_config,
            None,
        );
        let queue = dispatch2::DispatchQueue::new("dev.golive.sck", None);
        if stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                &output_proto,
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .is_err()
        {
            fail(PlatformError::Internal("saída de captura recusada".into()));
            return;
        }
        let started = std::sync::mpsc::channel::<Result<(), (String, i32)>>();
        let started_tx = started.0;
        let block = block2::RcBlock::new(move |error: *mut NSError| {
            if error.is_null() {
                let _ = started_tx.send(Ok(()));
            } else {
                // Non-null per check; borrowed for two getters.
                let error: &NSError = &*error;
                let _ = started_tx.send(Err(ns_error_parts(error)));
            }
        });
        stream.startCaptureWithCompletionHandler(Some(&block));
        // Rendezvous: started, typed error, or timeout (never hang).
        match started.1.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err((domain, code))) => {
                fail(map_ns_error(&domain, code));
                return;
            }
            Err(_) => {
                fail(PlatformError::Internal("timeout ao iniciar stream".into()));
                return;
            }
        }
        let _ = ready_tx.send(Ok(()));
        let mut last_capture = (applied.width, applied.height);
        while !stop_flag.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(50));
            if let Some(window) = tracked_window.as_ref() {
                let (nw, nh) = window_even_size(window);
                let next = fit_capture_size(nw, nh, &config);
                if (next.width, next.height) != last_capture {
                    stream_config.setWidth(next.width as usize);
                    stream_config.setHeight(next.height as usize);
                    stream.updateConfiguration_completionHandler(&stream_config, None);
                    last_capture = (next.width, next.height);
                }
            }
        }
        let stopped = std::sync::mpsc::channel::<()>();
        let stopped_tx = stopped.0;
        let stop_block = block2::RcBlock::new(move |_error: *mut NSError| {
            let _ = stopped_tx.send(());
        });
        stream.stopCaptureWithCompletionHandler(Some(&stop_block));
        let _ = stopped.1.recv_timeout(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera_fixture() -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Camera,
            id: "1".into(),
            name: "Test camera".into(),
            w: 1280,
            h: 720,
        }
    }

    fn display_fixture() -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Display,
            id: "42".into(),
            name: "Test display".into(),
            w: 1920,
            h: 1080,
        }
    }

    #[test]
    fn camera_scoped_discovery_never_calls_screen_enumerator() {
        let camera = camera_fixture();
        let listed = enumerate_kind_with(
            SourceKind::Camera,
            || panic!("camera listing must not touch ScreenCaptureKit"),
            || vec![camera.clone()],
        )
        .unwrap();
        assert_eq!(listed, vec![camera.clone()]);
    }

    #[test]
    fn combined_discovery_preserves_camera_on_screen_denial_only() {
        let camera = camera_fixture();
        let listed = enumerate_with(
            || Err(PlatformError::permission_denied()),
            || vec![camera.clone()],
        )
        .unwrap();
        assert_eq!(listed, vec![camera.clone()]);

        let error = enumerate_with(
            || Err(PlatformError::Internal("screen failure".into())),
            || vec![camera],
        );
        assert!(
            matches!(error, Err(PlatformError::Internal(message)) if message == "screen failure")
        );
    }

    #[test]
    fn scoped_screen_discovery_filters_kind_and_preserves_errors() {
        let listed = enumerate_kind_with(
            SourceKind::Display,
            || Ok(vec![display_fixture(), camera_fixture()]),
            || panic!("screen listing must not discover cameras"),
        )
        .unwrap();
        assert_eq!(listed, vec![display_fixture()]);
        assert!(matches!(
            enumerate_kind_with(
                SourceKind::Window,
                || Err(PlatformError::Internal("sck failure".into())),
                Vec::new,
            ),
            Err(PlatformError::Internal(message)) if message == "sck failure"
        ));
    }

    #[test]
    fn product_version_parses_plist_fixture() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>ProductVersion</key>
    <string>14.2.1</string>
</dict>
</plist>"#;
        assert_eq!(parse_product_version(plist), Some((14, 2)));
        assert_eq!(parse_product_version("garbage"), None);
        assert_eq!(parse_product_version("<key>ProductVersion</key>"), None);
    }

    #[test]
    fn os_version_detects_this_mac() {
        // Runs on any real Mac: sane values, never panics.
        let (major, _) = ScSource::os_version();
        assert!(major >= 12, "unexpected major {major}");
    }

    #[test]
    fn denial_maps_typed() {
        assert_eq!(
            map_ns_error("com.apple.ScreenCaptureKit.scstream.error", -3810),
            PlatformError::permission_denied()
        );
        // macOS 26 TCC code, inside and outside the SCK domain.
        assert_eq!(
            map_ns_error("com.apple.ScreenCaptureKit.scstream.error", -3801),
            PlatformError::permission_denied()
        );
        assert_eq!(
            map_ns_error("com.apple.TCC", -3801),
            PlatformError::permission_denied()
        );
        // Neighboring codes stay Internal (redacted domain + code).
        match map_ns_error("com.apple.ScreenCaptureKit.scstream.error", -3800) {
            PlatformError::Internal(detail) => {
                assert!(detail.contains("-3800"));
            }
            other => panic!("unexpected {other:?}"),
        }
        match map_ns_error("com.apple.Foo", 42) {
            PlatformError::Internal(detail) => {
                assert!(detail.contains("com.apple.Foo"));
                assert!(detail.contains("42"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn version_gate_maps_typed() {
        let mapped = PlatformError::OsVersionTooOld {
            have: "macOS 12".into(),
            need: "macOS 99",
        };
        match mapped {
            PlatformError::OsVersionTooOld { need, .. } => assert_eq!(need, "macOS 99"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn open_validates_without_os() {
        let bad = SourceInfo {
            kind: SourceKind::Display,
            id: "  ".into(),
            name: String::new(),
            w: 0,
            h: 0,
        };
        assert!(ScSource::open(&bad).is_err());
        let good = SourceInfo {
            kind: SourceKind::Window,
            id: "42".into(),
            name: String::new(),
            w: 0,
            h: 0,
        };
        assert!(ScSource::open(&good).is_ok());
    }

    #[test]
    fn gate_opens_exactly_on_cadence() {
        let interval = 1_000_000_000u64 / 15; // 15 fps profile
                                              // Fresh streams accept the first frame immediately (init stamps
                                              // one interval in the past).
        assert!(gate_open(initial_last_ns(0, interval), 0, interval));
        assert!(gate_open(
            initial_last_ns(5_000_000, interval),
            5_000_000,
            interval
        ));
        assert!(!gate_open(0, interval - 1, interval));
        assert!(gate_open(0, interval, interval));
        assert!(gate_open(1_000, 1_000 + interval, interval));
    }

    #[test]
    fn gate_drops_120hz_bursts_to_profile_rate_without_copies() {
        // Aligned arrivals all pass (the gate enforces spacing, never
        // drops on-cadence frames)…
        let interval = 1_000_000_000u64 / 15;
        let mut last = initial_last_ns(0, interval);
        let mut at = Vec::new();
        let mut t = 0u64;
        while t < 1_000_000_000 {
            if gate_open(last, t, interval) {
                at.push(t);
                last = golive_platform::cadence::advance_capture_clock(last, t, interval);
            }
            t += interval;
        }
        assert_eq!(at.len(), 16, "every on-cadence arrival passes");
        for pair in at.windows(2) {
            assert!(pair[1] - pair[0] >= interval, "spacing holds");
        }
        // …while a 120 Hz flood never exceeds it (grid quantization lands
        // slightly under: the gate caps, never pads). Drops touch nothing —
        // no copy happens before the gate in the handler.
        let mut last = initial_last_ns(0, interval);
        let mut copies = 0u32;
        let mut dropped = 0u32;
        let mut t = 0u64;
        while t < 1_000_000_000 {
            if gate_open(last, t, interval) {
                copies += 1; // WOULD copy
                last = golive_platform::cadence::advance_capture_clock(last, t, interval);
            } else {
                dropped += 1; // dies pre-copy
            }
            t += 1_000_000_000 / 120;
        }
        assert!(copies <= 15, "{copies} accepted at most");
        assert!(copies >= 12, "{copies} accepted keeps liveness");
        assert_eq!(copies + dropped, 121, "every tick accounted");
    }

    #[test]
    fn stream_interval_math_matches_profile_fps() {
        // Same formula the stream builder uses: 1s/fps, floor 1 fps.
        let interval = |fps: u32| 1_000_000_000u64 / fps.max(1) as u64;
        assert_eq!(interval(30), 33_333_333);
        assert_eq!(interval(15), 66_666_666);
        assert_eq!(interval(0), 1_000_000_000);
    }

    #[test]
    fn materialize_rejects_null_buffer_without_touching_os() {
        // SAFETY: test-only null handle; the helper null-checks before any
        // CoreVideo call, so nothing is dereferenced.
        let gpu = unsafe {
            golive_platform::GpuPixelBuffer::from_raw(
                std::ptr::null_mut(),
                64,
                64,
                256,
                release_cv_pixel_buffer,
            )
        };
        assert!(materialize_gpu_buffer(&gpu).is_none());
    }

    #[test]
    fn route_frame_covers_all_six_combos() {
        use FrameRoute::*;
        // Off cadence always dies pre-copy, whatever the sample.
        assert_eq!(route_frame(false, true, true), Drop);
        assert_eq!(route_frame(false, true, false), Drop);
        assert_eq!(route_frame(false, false, true), Drop);
        assert_eq!(route_frame(false, false, false), Drop);
        // On cadence: submittable BGRA+IOSurface retains (zero copy)…
        assert_eq!(route_frame(true, true, true), RetainGpu);
        // …BGRA without backing copies once…
        assert_eq!(route_frame(true, true, false), CopyCpu);
        // …anything else drops (never misinterpreted, same as before).
        assert_eq!(route_frame(true, false, true), Drop);
        assert_eq!(route_frame(true, false, false), Drop);
    }

    #[test]
    fn mock_callback_sequence_counts_copies_not_drops() {
        // Drives the pure routing core the way the SCK callback does:
        // (gate, format, backing) per arrival; counts what WOULD copy.
        // A 120 Hz all-submittable burst at 15 fps: 0 copies, all retained
        // or dropped pre-copy.
        let interval = 1_000_000_000u64 / 15;
        let mut last = initial_last_ns(0, interval);
        let (mut retained, mut copied, mut dropped) = (0u32, 0u32, 0u32);
        let mut t = 0u64;
        while t < 1_000_000_000 {
            let gate = gate_open(last, t, interval);
            match route_frame(gate, true, true) {
                FrameRoute::RetainGpu => {
                    retained += 1;
                    last = golive_platform::cadence::advance_capture_clock(last, t, interval);
                }
                FrameRoute::CopyCpu => {
                    copied += 1;
                    last = golive_platform::cadence::advance_capture_clock(last, t, interval);
                }
                FrameRoute::Drop => dropped += 1,
            }
            t += 1_000_000_000 / 120;
        }
        assert_eq!(copied, 0, "submittable burst never copies");
        assert!(retained <= 15 && retained >= 12, "{retained} retained");
        assert!(dropped > 100, "{dropped} died pre-copy");
        // Same burst, nothing IOSurface-backed: every accepted frame
        // copies exactly once (the fallback), drops still cost nothing.
        let mut last = initial_last_ns(0, interval);
        let (mut copied, mut dropped) = (0u32, 0u32);
        let mut t = 0u64;
        while t < 1_000_000_000 {
            let gate = gate_open(last, t, interval);
            match route_frame(gate, true, false) {
                FrameRoute::CopyCpu => {
                    copied += 1;
                    last = golive_platform::cadence::advance_capture_clock(last, t, interval);
                }
                FrameRoute::Drop => dropped += 1,
                FrameRoute::RetainGpu => panic!("nothing submittable here"),
            }
            t += 1_000_000_000 / 120;
        }
        assert!(copied <= 15 && copied >= 12, "{copied} fallback copies");
        assert!(dropped > 100, "{dropped} died pre-copy");
    }
}

pub mod decode;

mod frame_status;

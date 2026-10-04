//! Webcam capture via nokhwa (MediaFoundation on Windows).
//!
//! Mirrored by `platform-macos/src/camera.rs` adapter-for-adapter: identical
//! contract mapping, per-OS crate ownership (no shared capture dep by
//! design — see AGENTS.md §1). Keep the two in sync when changing the
//! format ladder, error mapping, or pump semantics.
//!
//! COM isolation (Windows): MediaFoundation initializes COM itself and
//! fails with `RPC_E_CHANGED_MODE` on threads running our MTA setup, so
//! every entry point here runs on threads that never call `init_com`
//! (fresh enum/still threads in `lib.rs`, the capture worker skips it).
//! The device is always opened AND used on the same thread (STA affinity).
//!
//! Mapping to the pure contract (`golive-platform`):
//! - `enumerate_cameras()` lists OS camera indices as `SourceKind::Camera`.
//!   A query failure degrades to an empty camera list (the screen list still
//!   stands); denial surfaces typed at open/start time, never as silence.
//! - `run_camera()` pumps decoded RGB frames swizzled to tight BGRA on a
//!   latest-only channel (same shape as the DXGI/WGC pumps).
//! - `thumbnail_camera()` grabs one frame for the share-modal preview.
//!
//! Frames out, errors typed. Device names, paths, pixels and driver strings
//! never reach logs or error text (static copy only — see `map_open_error`).

use golive_platform::{
    BgraFrame, CaptureConfig, CapturePacket, PixelFormat, PlatformError, SourceInfo, SourceKind,
};
use nokhwa::{
    pixel_format::RgbFormat,
    query,
    utils::{ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType},
    Camera, NokhwaError,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// How long `run_camera` waits for the first decoded frame before reporting.
const FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(8);
/// Consecutive unreadable frames before the pump reports failure.
const MAX_CONSECUTIVE_ERRORS: u32 = 30;

/// Opaque id scheme: the OS index rendered as text (`CameraIndex::as_string`,
/// e.g. `"0"`). Parses back numeric-first, string fallback (IP cameras).
pub fn parse_index(id: &str) -> Result<CameraIndex, PlatformError> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err(PlatformError::InvalidSource { reason: "id vazio" });
    }
    match trimmed.parse::<u32>() {
        Ok(n) => Ok(CameraIndex::Index(n)),
        Err(_) => Ok(CameraIndex::String(trimmed.to_owned())),
    }
}

/// Lists webcams as `SourceKind::Camera`. Query failure degrades to empty
/// (the caller appends to the screen list, which owns the denied-vs-empty
/// verdict). Names are user-facing display text only.
pub fn enumerate_cameras() -> Vec<SourceInfo> {
    match query(ApiBackend::Auto) {
        Ok(devices) => devices
            .into_iter()
            .map(|info| {
                let human = info.human_name();
                let name = if human.trim().is_empty() {
                    format!("Webcam {}", info.index().as_string())
                } else {
                    human
                };
                SourceInfo {
                    kind: SourceKind::Camera,
                    id: info.index().as_string(),
                    name,
                    // Listing carries no negotiated geometry; the open path
                    // reports real dims per frame. 720p keeps profile fitting
                    // honest until the first frame arrives.
                    w: 1280,
                    h: 720,
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Requested capture formats, in preference order: 720p then 480p MJPEG
/// (cheap to decode), 480p YUYV (uncompressed fallback), then the fastest
/// available mode. Real-time modes win: a 720p@1 mode loses to 480p@30 for
/// streaming. The bridge fits into the share profile anyway.
fn requested_formats(config: &CaptureConfig) -> Vec<RequestedFormat<'static>> {
    let fps = config.fps.min(30).max(1);
    vec![
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new_from(1280, 720, FrameFormat::MJPEG, fps),
        )),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new_from(640, 480, FrameFormat::MJPEG, fps),
        )),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new_from(640, 480, FrameFormat::YUYV, fps),
        )),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate),
    ]
}

/// Minimum negotiated fps worth streaming (below this the picture is a
/// slideshow; later candidates and the anything-goes fallback follow).
const MIN_STREAM_FPS: u32 = 10;

fn open_camera(id: &str, config: &CaptureConfig) -> Result<Camera, PlatformError> {
    let index = parse_index(id)?;
    let mut last_error: Option<NokhwaError> = None;
    let formats = requested_formats(config);
    for (position, requested) in formats.iter().enumerate() {
        let last = position + 1 == formats.len();
        match Camera::new(index.clone(), *requested) {
            Ok(camera) => {
                if last || camera.camera_format().frame_rate() >= MIN_STREAM_FPS {
                    return Ok(camera);
                }
                last_error = Some(NokhwaError::GeneralError("fps baixo".into()));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(map_open_error(id, &last_error.unwrap_or(NokhwaError::UnitializedError)))
}

/// Open/start failure → typed error. Raw driver strings never surface (they
/// may carry device paths); only static copy plus the opaque id travel.
fn map_open_error(id: &str, error: &NokhwaError) -> PlatformError {
    let text = error.to_string().to_lowercase();
    if text.contains("denied") || text.contains("permission") || text.contains("access") {
        return PlatformError::camera_permission_denied();
    }
    if text.contains("busy") || text.contains("taken") || text.contains("in use") {
        return PlatformError::Internal("webcam em uso por outro app".into());
    }
    if text.contains("not found")
        || text.contains("no device")
        || text.contains("invalid")
        || text.contains("disconnected")
    {
        return PlatformError::SourceGone { id: id.to_owned() };
    }
    if text.contains("fulfill") || text.contains("format") {
        return PlatformError::Internal("webcam sem modo de vídeo compatível".into());
    }
    match error {
        NokhwaError::UnsupportedOperationError(_) => PlatformError::Internal(
            "webcam não suportada neste sistema".into(),
        ),
        _ => PlatformError::Internal("webcam indisponível".into()),
    }
}

/// RGB (3 bytes/px, row-major) → tight BGRA. `None` on size mismatch — the
/// caller skips the frame instead of panicking. Pure.
fn rgb_to_bgra(w: u32, h: u32, rgb: Vec<u8>) -> Option<BgraFrame> {
    if w == 0 || h == 0 {
        return None;
    }
    let pixels = (w as usize).checked_mul(h as usize)?;
    if rgb.len() != pixels.checked_mul(3)? {
        return None;
    }
    let mut data = vec![0u8; pixels * 4];
    for (dst, src) in data.chunks_exact_mut(4).zip(rgb.chunks_exact(3)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = 255;
    }
    Some(BgraFrame { w, h, stride: (w as usize) * 4, format: PixelFormat::Bgra8888, data })
}

fn decode_buffer(
    buffer: &nokhwa::Buffer,
) -> Option<BgraFrame> {
    let resolution = buffer.resolution();
    let (w, h) = (resolution.width(), resolution.height());
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    let rgb = buffer.decode_image::<RgbFormat>().ok()?.into_raw();
    rgb_to_bgra(w, h, rgb)
}

/// Pump body: opens the device, rendezvouses on the first frame, then feeds
/// latest-only until `stop` fires or the device fails. Mirrors the
/// DXGI/WGC worker shape (`ready_tx` rendezvous, `error` slot, bounded drop).
pub fn run_camera(
    id: String,
    config: CaptureConfig,
    frame_tx: mpsc::SyncSender<CapturePacket>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<PlatformError>>>,
    ready_tx: mpsc::Sender<Result<(), PlatformError>>,
) {
    let mut camera = match open_camera(&id, &config) {
        Ok(camera) => camera,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    if let Err(error) = camera.open_stream().map_err(|e| map_open_error(&id, &e)) {
        let _ = ready_tx.send(Err(error));
        return;
    }
    let deadline = Instant::now() + FIRST_FRAME_DEADLINE;
    let mut ready_sent = false;
    let mut consecutive_errors = 0u32;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match camera.frame() {
            Ok(buffer) => {
                consecutive_errors = 0;
                let Some(frame) = decode_buffer(&buffer) else {
                    continue;
                };
                if !ready_sent {
                    ready_sent = true;
                    let _ = ready_tx.send(Ok(()));
                }
                let _ = frame_tx.try_send(CapturePacket::Cpu(frame));
            }
            Err(frame_error) => {
                consecutive_errors += 1;
                if !ready_sent && Instant::now() >= deadline {
                    let _ = ready_tx.send(Err(map_open_error(&id, &frame_error)));
                    return;
                }
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    if let Ok(mut guard) = error.lock() {
                        *guard = Some(PlatformError::Internal("leitura da webcam falhou".into()));
                    }
                    if !ready_sent {
                        let _ = ready_tx.send(Err(PlatformError::Internal(
                            "leitura da webcam falhou".into(),
                        )));
                    }
                    break;
                }
                // Pace failures: a dead device reports in ~3s instead of
                // burning the budget in a hot microsecond loop; transient
                // blips (a second of bad frames) ride through.
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let _ = camera.stop_stream();
}

/// One-shot still for the share-modal preview. Opens, grabs one frame with a
/// deadline, closes. Typed failures; never logs device names/pixels.
pub fn thumbnail_camera(id: &str) -> Result<BgraFrame, PlatformError> {
    let config = CaptureConfig { width: 640, height: 480, fps: 15 };
    let mut camera =
        open_camera(id, &config).and_then(|mut camera| {
            camera
                .open_stream()
                .map_err(|error| map_open_error(id, &error))
                .map(|()| camera)
        })?;
    let deadline = Instant::now() + FIRST_FRAME_DEADLINE;
    loop {
        match camera.frame() {
            Ok(buffer) => {
                let _ = camera.stop_stream();
                return decode_buffer(&buffer)
                    .ok_or_else(|| PlatformError::Internal("thumbnail vazio".into()));
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    let _ = camera.stop_stream();
                    return Err(map_open_error(id, &error));
                }
                // Don't hot-spin a dead device while waiting out the deadline.
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_id_roundtrips_numeric_first() {
        assert_eq!(parse_index("0").unwrap().as_string(), "0");
        assert_eq!(parse_index("  2 ").unwrap().as_string(), "2");
        assert_eq!(parse_index("ipc-cam").unwrap().as_string(), "ipc-cam");
        assert!(parse_index("").is_err());
        assert!(parse_index("   ").is_err());
    }

    #[test]
    fn rgb_swizzles_to_bgra_opaque() {
        // One red + one green pixel: R/G swap, B mirrored, alpha forced.
        let frame = rgb_to_bgra(2, 1, vec![255, 0, 0, 0, 255, 0]).unwrap();
        assert_eq!((frame.w, frame.h, frame.stride), (2, 1, 8));
        assert_eq!(
            frame.data,
            vec![0, 0, 255, 255, 0, 255, 0, 255],
        );
        // Size mismatch never panics — the pump skips the frame.
        assert!(rgb_to_bgra(2, 1, vec![1, 2, 3]).is_none());
        assert!(rgb_to_bgra(0, 0, Vec::new()).is_none());
    }

    #[test]
    fn open_errors_stay_typed_without_driver_text() {
        // Simulated OS denial (message shape mirrors MF E_ACCESSDENIED).
        let denied = map_open_error(
            "0",
            &NokhwaError::OpenDeviceError("0".into(), "access denied (0x80070005)".into()),
        );
        assert!(matches!(denied, PlatformError::PermissionDenied { .. }));
        assert!(denied.to_string().contains("Câmera"));
        assert!(!denied.to_string().contains("0x80070005"));
        // Busy device names its own remedy, never the raw string.
        let busy = map_open_error(
            "0",
            &NokhwaError::OpenStreamError("device is busy".into()),
        );
        assert_eq!(busy.to_string(), "falha de captura: webcam em uso por outro app");
        // Gone device keeps the opaque id (numeric — no titles exist here).
        let gone = map_open_error(
            "3",
            &NokhwaError::OpenDeviceError("3".into(), "device not found".into()),
        );
        assert!(matches!(gone, PlatformError::SourceGone { .. }));
    }

    #[test]
    fn requested_prefers_720p30_mjpeg_with_fallback() {
        let config = CaptureConfig { width: 1920, height: 1080, fps: 60 };
        let formats = requested_formats(&config);
        assert_eq!(formats.len(), 4);
        assert!(matches!(
            formats[3].requested_format_type(),
            RequestedFormatType::AbsoluteHighestFrameRate
        ));
        // Incompatible virtual devices fail typed, without driver text.
        let incompatible = map_open_error(
            "0",
            &NokhwaError::OpenDeviceError("0".into(), "Failed to fulfill requested format".into()),
        );
        assert_eq!(
            incompatible.to_string(),
            "falha de captura: webcam sem modo de vídeo compatível"
        );
    }
}

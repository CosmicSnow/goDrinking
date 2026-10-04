//! Scriptable source with no OS involved. Core and app tests inject frames
//! through this instead of touching the screen.

use crate::error::PlatformError;
use crate::traits::{FrameStream, NextError, VideoSource};
use crate::types::{BgraFrame, CaptureConfig, CapturePacket, PixelFormat, SourceInfo, SourceKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// A fake source: scripted frames, scripted failures. `frames` cycles
/// forever; `fail_after` injects [`PlatformError`] instead of ending.
#[derive(Clone, Debug)]
pub struct MockSource {
    pub info: SourceInfo,
    pub frames: Vec<BgraFrame>,
    pub fail_after: Option<(usize, PlatformError)>,
}

impl MockSource {
    pub fn display(id: &str, w: u32, h: u32) -> Self {
        Self {
            info: SourceInfo {
                kind: SourceKind::Display,
                id: id.into(),
                name: format!("Mock display {id}"),
                w,
                h,
            },
            frames: Vec::new(),
            fail_after: None,
        }
    }

    pub fn camera(id: &str, w: u32, h: u32) -> Self {
        Self {
            info: SourceInfo {
                kind: SourceKind::Camera,
                id: id.into(),
                name: format!("Mock webcam {id}"),
                w,
                h,
            },
            frames: Vec::new(),
            fail_after: None,
        }
    }

    pub fn with_solid_frame(mut self, r: u8, g: u8, b: u8) -> Self {
        let (w, h) = (self.info.w.max(2), self.info.h.max(2));
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        self.frames.push(BgraFrame {
            w,
            h,
            stride: (w * 4) as usize,
            format: PixelFormat::Bgra8888,
            data,
        });
        self
    }
}

impl VideoSource for MockSource {
    fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
        Ok(vec![MockSource::display("mock-1", 1280, 720).info])
    }

    fn open(info: &SourceInfo) -> Result<Self, PlatformError> {
        if info.id.trim().is_empty() {
            return Err(PlatformError::InvalidSource { reason: "id vazio" });
        }
        Ok(Self {
            info: info.clone(),
            frames: Vec::new(),
            fail_after: None,
        })
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<FrameStream, PlatformError> {
        // Empty script is legal: the worker ends at once (Ended, not hang).
        let frames = self.frames.clone();
        let fail_after = self.fail_after.clone();
        let fps = config.fps.max(1);
        let (tx, rx) = mpsc::sync_channel::<CapturePacket>(2);
        let error: Arc<std::sync::Mutex<Option<PlatformError>>> = Arc::new(std::sync::Mutex::new(None));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let error_ = Arc::clone(&error);
        let stop_ = Arc::clone(&stop_flag);
        let worker = std::thread::Builder::new()
            .name("golive-mock-source".into())
            .spawn(move || {
                if frames.is_empty() {
                    // Nothing scripted: end immediately (Ended, not hang).
                    return;
                }
                let tick = Duration::from_millis((1000 / fps.min(60) as u64).max(1));
                let mut n = 0usize;
                while !stop_.load(Ordering::Acquire) {
                    if let Some((after, error)) = &fail_after {
                        if n >= *after {
                            if let Ok(mut guard) = error_.lock() {
                                *guard = Some(error.clone());
                            }
                            break;
                        }
                    }
                    let frame = frames[n % frames.len()].clone();
                    n += 1;
                    if tx.send(CapturePacket::Cpu(frame)).is_err() {
                        break;
                    }
                    std::thread::sleep(tick);
                }
            })
            .map_err(|e| PlatformError::Internal(format!("thread do mock: {e}")))?;
        Ok(FrameStream::new(rx, error, stop_flag, worker))
    }
}

/// Drives an already-opened source: N frames, then stop. Shared by backend
/// tests so lifecycle semantics stay identical everywhere.
pub fn drive_lifecycle<S: VideoSource>(
    mut source: S,
    frames_wanted: usize,
) -> Result<Vec<BgraFrame>, PlatformError> {
    let mut stream = source.start(&CaptureConfig::default())?;
    let mut out = Vec::new();
    for _ in 0..frames_wanted {
        match stream.next_frame(Duration::from_secs(5)) {
            // The mock only ever emits CPU packets; a GPU packet here
            // would be a mock bug, surfaced loudly instead of silently.
            Ok(CapturePacket::Cpu(frame)) => out.push(frame),
            Ok(CapturePacket::Gpu(_)) => panic!("mock emitted a GPU packet"),
            Err(NextError::Timeout) => continue,
            Err(NextError::Ended) => break,
            Err(NextError::Failed(error)) => {
                let _ = stream.stop(Duration::from_secs(2));
                return Err(error);
            }
        }
    }
    stream.stop(Duration::from_secs(5))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::RestartOrder;

    fn info() -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Display,
            id: "mock-1".into(),
            name: "Mock".into(),
            w: 64,
            h: 64,
        }
    }

    #[test]
    fn enumerate_lists_one_display() {
        let list = MockSource::enumerate().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, SourceKind::Display);
    }

    #[test]
    fn camera_mock_opens_and_streams_like_any_kind() {
        let info = MockSource::camera("cam-0", 64, 64).info;
        assert_eq!(info.kind, SourceKind::Camera);
        let mut source = MockSource::open(&info).unwrap();
        source.frames = vec![MockSource::camera("x", 64, 64)
            .with_solid_frame(10, 20, 30)
            .frames
            .remove(0)];
        let frames = drive_lifecycle(source, 2).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].w, frames[0].h), (64, 64));
    }

    #[test]
    fn open_rejects_empty_id() {
        let mut bad = info();
        bad.id.clear();
        assert!(MockSource::open(&bad).is_err());
    }

    #[test]
    fn restart_order_defaults_to_new_first() {
        // Backends without single-stream limits keep the glitch-free order;
        // only constrained backends (Windows DXGI displays) override.
        assert_eq!(MockSource::restart_order(&info()), RestartOrder::NewFirst);
    }

    #[test]
    fn lifecycle_delivers_and_stops() {
        let info = info();
        let mut source = MockSource::open(&info).unwrap();
        source.frames = vec![MockSource::display("x", 64, 64)
            .with_solid_frame(10, 20, 30)
            .frames
            .remove(0)];
        let frames = drive_lifecycle(source, 3).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!((frames[0].w, frames[0].h), (64, 64));
    }

    #[test]
    fn empty_script_ends_immediately() {
        let info = info();
        let mut source = MockSource::open(&info).unwrap();
        let stream = source.start(&CaptureConfig::default()).unwrap();
        assert!(matches!(
            stream.next_frame(Duration::from_secs(5)),
            Err(NextError::Ended)
        ));
    }

    #[test]
    fn scripted_failure_surfaces_typed() {
        let info = info();
        let mut source = MockSource::open(&info).unwrap();
        source.frames = vec![MockSource::display("x", 2, 2).with_solid_frame(1, 2, 3).frames.remove(0)];
        source.fail_after = Some((1, PlatformError::SourceGone { id: "9".into() }));
        let stream = source.start(&CaptureConfig::default()).unwrap();
        assert!(stream.next_frame(Duration::from_secs(5)).is_ok());
        assert!(matches!(
            stream.next_frame(Duration::from_secs(5)),
            Err(NextError::Failed(PlatformError::SourceGone { id }))
            if id == "9"
        ));
    }
}

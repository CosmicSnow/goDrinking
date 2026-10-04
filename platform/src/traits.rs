//! The capture contract. Backends implement this; the core never sees it
//! (packets cross into the core as plain channel data), and tests inject a
//! [`crate::mock::MockSource`] with no OS involved.

use crate::error::PlatformError;
use crate::types::{CaptureConfig, CapturePacket, SourceInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

/// What `next_frame` can report besides a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NextError {
    /// Nothing arrived within the budget — tick, don't treat as failure.
    Timeout,
    /// The source ended (or the backend shut down). Stop asking.
    Ended,
    /// The backend failed asynchronously (e.g. revoked mid-stream).
    Failed(PlatformError),
}

/// A live capture: poll frames, stop with a deadline. Dropping stops
/// best-effort (bounded); explicit [`FrameStream::stop`] reports.
pub struct FrameStream {
    rx: mpsc::Receiver<CapturePacket>,
    error: Arc<std::sync::Mutex<Option<PlatformError>>>,
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    capture_probe: Option<Arc<crate::capture_probe::CaptureProbe>>,
}

impl FrameStream {
    /// Backend-only constructor: backends build the channel + worker and
    /// hand the assembled stream over. Not for app code (use a backend).
    pub fn new(
        rx: mpsc::Receiver<CapturePacket>,
        error: Arc<std::sync::Mutex<Option<PlatformError>>>,
        stop_flag: Arc<AtomicBool>,
        worker: JoinHandle<()>,
    ) -> Self {
        Self {
            rx,
            error,
            stop_flag,
            worker: Some(worker),
            capture_probe: None,
        }
    }

    pub fn with_capture_probe(mut self, probe: Arc<crate::capture_probe::CaptureProbe>) -> Self {
        self.capture_probe = Some(probe);
        self
    }

    pub fn take_capture_counts(&self) -> Option<crate::capture_probe::CaptureCounts> {
        self.capture_probe.as_ref().map(|probe| probe.take())
    }

    /// Next packet within `timeout`. Stale packets never queue upstream: the
    /// backend keeps latest-only (cap 2, drop oldest), so this is fresh.
    pub fn next_frame(&self, timeout: Duration) -> Result<CapturePacket, NextError> {
        match self.rx.recv_timeout(timeout) {
            Ok(frame) => Ok(frame),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(NextError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Ok(guard) = self.error.lock() {
                    if let Some(error) = guard.clone() {
                        return Err(NextError::Failed(error));
                    }
                }
                Err(NextError::Ended)
            }
        }
    }

    /// Deterministic stop with deadline: signal, join bounded, then release.
    /// Never wedges — a stuck backend is abandoned after `deadline`.
    pub fn stop(&mut self, deadline: Duration) -> Result<(), PlatformError> {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if join_deadline(worker, deadline) {
                Ok(())
            } else {
                Err(PlatformError::Internal("stop excedeu o deadline".into()))
            }
        } else {
            Ok(())
        }
    }

    /// Shared flag backends poll to exit promptly.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_flag)
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        let _ = self.stop(Duration::from_secs(2));
    }
}

fn join_deadline(worker: JoinHandle<()>, deadline: Duration) -> bool {
    let (done_tx, done_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = worker.join();
        let _ = done_tx.send(());
    });
    done_rx.recv_timeout(deadline).is_ok()
}

/// Stream-restart ordering for profile switches (`set_quality`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartOrder {
    /// Start the new stream first, retire the old one after: glitch-free
    /// (same channel, latest-only). Requires the backend to tolerate two
    /// concurrent streams on one source.
    NewFirst,
    /// Stop + join the old stream before spawning the new one: a brief
    /// blackout gap, but the only order backends with single-stream sources
    /// support (Windows DXGI allows one duplication per process per output —
    /// a second `DuplicateOutput` while the old one is alive fails
    /// E_INVALIDARG).
    StopFirst,
}

/// A capturable source family. Implemented per OS (macOS backend,
/// Windows stub, mock). All methods are synchronous and bounded; the
/// first call that needs the OS may trigger its permission prompt.
pub trait VideoSource: Send + Sized {
    /// List current sources. Empty (not error) is possible headless — but
    /// on a real desktop, denial hides content, so backends map
    /// empty-on-desktop to [`PlatformError::PermissionDenied`].
    fn enumerate() -> Result<Vec<SourceInfo>, PlatformError>;

    /// List one source family without touching unrelated platform APIs.
    /// Backends with independent permission domains should override this;
    /// the default preserves the historical all-sources behavior.
    fn enumerate_kind(kind: crate::types::SourceKind) -> Result<Vec<SourceInfo>, PlatformError> {
        Ok(Self::enumerate()?
            .into_iter()
            .filter(|source| source.kind == kind)
            .collect())
    }

    /// Validate a listed id/kind pair without starting capture.
    fn open(info: &SourceInfo) -> Result<Self, PlatformError>;

    /// Start capture. May trigger the OS permission prompt on first use.
    fn start(&mut self, config: &CaptureConfig) -> Result<FrameStream, PlatformError>;

    /// Start capture while observing a caller-owned cancellation signal.
    /// Existing backends keep their current bounded start path; adapters
    /// with asynchronous permission/setup can override this to cancel while
    /// rendezvousing without weakening their ownership guarantees.
    fn start_with_cancel(
        &mut self,
        config: &CaptureConfig,
        cancel: Arc<AtomicBool>,
    ) -> Result<FrameStream, PlatformError> {
        if cancel.load(Ordering::Acquire) {
            return Err(PlatformError::Internal("captura cancelada".into()));
        }
        self.start(config)
    }

    /// Restart ordering for a profile switch on one listed source. Default
    /// is the glitch-free [`RestartOrder::NewFirst`]; backends whose OS
    /// forbids concurrent streams on one source override per kind.
    fn restart_order(_info: &SourceInfo) -> RestartOrder {
        RestartOrder::NewFirst
    }
}

#[cfg(test)]
mod scoped_contract_tests {
    use super::*;
    use crate::mock::MockSource;
    use crate::types::SourceKind;

    #[test]
    fn default_scoped_enumeration_filters_the_common_list() {
        let sources = MockSource::enumerate_kind(SourceKind::Display).unwrap();
        assert!(!sources.is_empty());
        assert!(sources
            .iter()
            .all(|source| source.kind == SourceKind::Display));
        assert!(MockSource::enumerate_kind(SourceKind::Camera)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn default_start_honors_cancellation_before_opening() {
        let source = MockSource::enumerate().unwrap().remove(0);
        let mut source = MockSource::open(&source).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        assert!(source
            .start_with_cancel(&CaptureConfig::default(), cancel)
            .is_err());
    }
}

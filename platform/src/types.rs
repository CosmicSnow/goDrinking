//! Plain data types. No OS calls, no threads — safe to construct anywhere,
//! including tests and the UI layer.

use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use std::ptr::null_mut;

/// What can be captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Display,
    Window,
    Camera,
}

/// One capturable source, as listed for the UI. `id` is the OS handle
/// rendered opaque (decimal display/window id); `name` is user-facing text
/// (window titles may be sensitive — display it, never log it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub kind: SourceKind,
    pub id: String,
    pub name: String,
    pub w: u32,
    pub h: u32,
}

/// Requested output geometry + rate. Backends scale to fit; the core scales
/// again to the contract size, so this is a quality hint, not a promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Builds an output config already clamped to backend bounds: SCK rejects
/// degenerate or absurd sizes, so every caller funnels through here (pure,
/// tested). Floor 64 keeps tiny windows valid without the old 320x240
/// forced upscale; ceiling 8192 matches the encoder (5120×1440 fits); fps
/// 1..=60 feeds `minimumFrameInterval` directly.
pub const MAX_CAPTURE_DIM: u32 = 8192;

pub fn capture_config_for(w: u32, h: u32, fps: u32) -> CaptureConfig {
    CaptureConfig {
        width: w.clamp(64, MAX_CAPTURE_DIM),
        height: h.clamp(64, MAX_CAPTURE_DIM),
        fps: fps.clamp(1, 60),
    }
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps: 30 }
    }
}

/// Declared pixel layout of [`BgraFrame`]. Only BGRA exists today; the enum
/// keeps future formats (P010, NV12) from becoming silent reinterpretations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Bgra8888,
}

/// One captured frame: packed BGRA rows. `stride >= w*4`; `data.len()` must
/// cover `stride*(h-1) + w*4`. No timestamps here — pacing belongs to the
/// consumer (the core paces by frame counter, never wall-clock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BgraFrame {
    pub w: u32,
    pub h: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

/// Planar 4:2:0 result: contiguous Y then U then V, no padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanarYuv {
    pub w: u32,
    pub h: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// Opaque retained GPU pixel buffer (macOS: a +1 CVPixelBuffer retained by
/// the capture backend straight out of the SCK callback — zero CPU copy).
///
/// Cross-platform shell on purpose: only macOS backends ever construct it;
/// every other crate just forwards or drops it. `Send` (no thread
/// affinity: IOSurfaces are process-global). `Drop` runs `release`,
/// balancing the +1 exactly once — latest-only eviction therefore never
/// leaks, and `take()` transfers ownership out (Drop goes inert).
/// Never log one: dims are metadata, contents are the user's screen.
pub struct GpuPixelBuffer {
    ptr: *mut c_void,
    release: Option<unsafe extern "C-unwind" fn(*mut c_void)>,
    pub w: u32,
    pub h: u32,
    pub stride: usize,
}

// SAFETY: the pointer is an opaque +1 reference with no thread affinity;
// ownership (and the single release) moves with the value.
unsafe impl Send for GpuPixelBuffer {}

impl Drop for GpuPixelBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            if let Some(release) = self.release {
                // SAFETY: constructed from a live +1 by `from_raw`.
                unsafe { release(self.ptr) };
            }
            self.ptr = null_mut();
        }
    }
}

impl std::fmt::Debug for GpuPixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuPixelBuffer")
            .field("w", &self.w)
            .field("h", &self.h)
            .field("stride", &self.stride)
            .finish()
    }
}

impl GpuPixelBuffer {
    /// Wrap a live +1 reference.
    ///
    /// # Safety
    /// `ptr` must be non-null and a +1 reference releasable by exactly one
    /// `release` call. After this returns, the +1 belongs to the value
    /// (or to whoever calls `take()`).
    pub unsafe fn from_raw(
        ptr: *mut c_void,
        w: u32,
        h: u32,
        stride: usize,
        release: unsafe extern "C-unwind" fn(*mut c_void),
    ) -> Self {
        Self { ptr, release: Some(release), w, h, stride }
    }

    /// Move the +1 out; Drop becomes inert. The caller owns the release.
    pub fn take(&mut self) -> *mut c_void {
        std::mem::replace(&mut self.ptr, null_mut())
    }

    /// Borrow the raw handle (the +1 stays owned here). For read-only
    /// inspection (format checks, locked copies) — never store it.
    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }
}

/// One captured packet: owned CPU pixels or a retained GPU buffer.
/// The capture backend picks per frame (GPU when the sample is directly
/// submittable, CPU copy otherwise); downstream routes without re-deciding.
#[derive(Debug)]
pub enum CapturePacket {
    Cpu(BgraFrame),
    Gpu(GpuPixelBuffer),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_info_roundtrips_for_tauri() {
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "1".into(),
            name: "Display 1 · 2560x1440".into(),
            w: 2560,
            h: 1440,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"display\""));
        let back: SourceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn capture_config_clamps_to_backend_bounds() {
        // Profile-sized requests pass through untouched.
        assert_eq!(
            capture_config_for(742, 480, 15),
            CaptureConfig { width: 742, height: 480, fps: 15 }
        );
        assert_eq!(
            capture_config_for(3456, 2234, 120),
            CaptureConfig { width: 3456, height: 2234, fps: 60 }
        );
        assert_eq!(
            capture_config_for(9000, 9000, 60),
            CaptureConfig { width: MAX_CAPTURE_DIM, height: MAX_CAPTURE_DIM, fps: 60 }
        );
        // Degenerate/zero floors without the old forced 320x240 upscale.
        assert_eq!(
            capture_config_for(0, 0, 0),
            CaptureConfig { width: 64, height: 64, fps: 1 }
        );
        assert_eq!(
            capture_config_for(200, 150, 30),
            CaptureConfig { width: 200, height: 150, fps: 30 }
        );
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    unsafe extern "C-unwind" fn mock_release(ptr: *mut c_void) {
        assert!(!ptr.is_null());
        (*(ptr as *const AtomicU64)).fetch_add(1, Ordering::SeqCst);
    }

    fn mock_gpu(count: &AtomicU64) -> GpuPixelBuffer {
        unsafe {
            GpuPixelBuffer::from_raw(
                count as *const AtomicU64 as *mut c_void,
                64,
                48,
                256,
                mock_release,
            )
        }
    }

    #[test]
    fn gpu_handle_releases_exactly_once_on_drop() {
        let count = AtomicU64::new(0);
        {
            let _gpu = mock_gpu(&count);
            assert_eq!(_gpu.w, 64);
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn gpu_handle_take_transfers_ownership_drop_goes_inert() {
        let count = AtomicU64::new(0);
        let mut gpu = mock_gpu(&count);
        let raw = gpu.take();
        assert!(!raw.is_null());
        drop(gpu);
        assert_eq!(count.load(Ordering::SeqCst), 0, "taken handle must not release");
        unsafe { mock_release(raw) };
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn gpu_handle_crosses_threads_like_the_capture_channel() {
        let count = AtomicU64::new(0);
        let (tx, rx) = std::sync::mpsc::sync_channel::<CapturePacket>(2);
        tx.send(CapturePacket::Gpu(mock_gpu(&count))).unwrap();
        drop(tx);
        let packet = rx.recv().unwrap();
        assert!(matches!(packet, CapturePacket::Gpu(_)));
        drop(packet);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

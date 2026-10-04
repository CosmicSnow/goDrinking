//! Capture platform abstraction: trait, types, converters, capabilities.
//!
//! ```text
//! golive-platform          pure: VideoSource trait, SourceInfo, BgraFrame,
//!                          PlatformError, CapabilitySet, converters, mock,
//!                          windows stub. Zero OS dependencies.
//! golive-platform-macos    macOS only: ScreenCaptureKit backend.
//! golive-core              consumes injected frames (std channel) — never
//!                          imports this crate or any OS bindings.
//! golive-app               wires backend → bridge → core.
//! ```
//!
//! Rules: adapters stay small (frames out, errors typed — no RTP/WebRTC/
//! lifecycle in backends); titles never enter logs; denial is a typed error
//! with user copy, never silence.

pub mod audio;
pub mod capability;
pub mod cadence;
pub mod capture_probe;
pub mod convert;
pub mod error;
pub mod mock;
pub mod traits;
pub mod types;
#[cfg(target_os = "windows")]
pub mod windows;

pub use audio::{
    app_excluded_by_token, default_excluded_tokens, AudioApp, EncodedAudioPacket,
    DEFAULT_EXCLUDED_TOKENS,
};
pub use capability::{capabilities, CapabilitySet, Support};
pub use convert::{bgra_to_i420, convert_error, ConvertError};
pub use error::{PlatformError, CAMERA_PERMISSION_HINT, PERMISSION_HINT};
pub use traits::{FrameStream, NextError, RestartOrder, VideoSource};
pub use types::{capture_config_for, BgraFrame, CaptureConfig, CapturePacket, GpuPixelBuffer, PixelFormat, PlanarYuv, SourceInfo, SourceKind};

pub mod decode;

//! What the current platform can capture. Compile-time facts with a
//! human-readable `reason` — the UI disables options with the explanation
//! instead of failing silently. Runtime denial (TCC) still surfaces as
//! [`crate::PlatformError::PermissionDenied`] at enumerate/start time.

use serde::Serialize;

/// One capability verdict. `Serialize`-only by design: capabilities flow
/// one way (backend → UI); borrowed `&'static` reasons stay zero-copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Support {
    pub supported: bool,
    pub reason: &'static str,
}

#[allow(dead_code)]
const fn yes(reason: &'static str) -> Support {
    Support { supported: true, reason }
}

#[allow(dead_code)]
const fn no(reason: &'static str) -> Support {
    Support { supported: false, reason }
}

/// The four axes the UI cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CapabilitySet {
    pub display: Support,
    pub window: Support,
    pub camera: Support,
    pub app_audio: Support,
    pub exclusion: Support,
}

/// Compile-time capabilities for this build. Audio capture is a process tap
/// (macOS 14.2+) or WASAPI loopback (Windows); exclusion is audio-only.
pub const fn capabilities() -> CapabilitySet {
    #[cfg(target_os = "macos")]
    {
        CapabilitySet {
            display: yes("ScreenCaptureKit (pode pedir permissão no primeiro uso)"),
            window: yes("ScreenCaptureKit (pode pedir permissão no primeiro uso)"),
            camera: yes("webcam via AVFoundation (pode pedir permissão no primeiro uso)"),
            app_audio: yes("process tap (macOS 14.2+)"),
            exclusion: yes("CATapDescription exclui apps do áudio, sem esconder a janela"),
        }
    }
    #[cfg(target_os = "windows")]
    {
        CapabilitySet {
            display: yes("DXGI Desktop Duplication"),
            window: yes("Windows.Graphics.Capture (pode pedir permissão no primeiro uso)"),
            camera: yes("webcam via MediaFoundation (pode pedir permissão no primeiro uso)"),
            app_audio: yes("WASAPI loopback"),
            exclusion: yes("process loopback exclui cada app marcado"),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        CapabilitySet {
            display: no("apenas macOS/Windows"),
            window: no("apenas macOS/Windows"),
            camera: no("apenas macOS/Windows"),
            app_audio: no("apenas macOS/Windows"),
            exclusion: no("apenas macOS/Windows"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_never_empty() {
        let caps = capabilities();
        for support in [caps.display, caps.window, caps.camera, caps.app_audio, caps.exclusion] {
            assert!(!support.reason.is_empty());
        }
    }

    #[test]
    fn app_audio_is_supported_on_desktop() {
        let caps = capabilities();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert!(caps.app_audio.supported);
            assert!(caps.exclusion.supported);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            assert!(!caps.app_audio.supported);
            assert!(!caps.exclusion.supported);
        }
    }
}

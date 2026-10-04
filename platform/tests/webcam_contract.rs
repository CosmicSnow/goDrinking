//! TDD RED — contrato webcam no backend (feature/webcam-detect-stream).
//!
//! Uso real coberto:
//! - `SourceKind::Camera` serializa `"camera"` (fio Tauri/UI, lowercase como display/window)
//! - `capabilities()` expõe eixo `camera` com motivo honesto
//! - `ShareSource::parse("camera:<id>")` aceita id real, rejeita vazio
//! - `SourceInfo { kind: Camera }` faz roundtrip JSON para o Tauri
//!
//! Estes testes FALHAM hoje: `SourceKind` só tem Display/Window,
//! `CapabilitySet` só tem display/window/app_audio/exclusion,
//! `ShareSource` só aceita synthetic/movie/display/window.

use golive_platform::{CapabilitySet, SourceInfo, SourceKind};

#[test]
fn camera_kind_serializes_lowercase_for_tauri_wire() {
    let info = SourceInfo {
        kind: SourceKind::Camera,
        id: "0".to_owned(),
        name: "Webcam".to_owned(),
        w: 1280,
        h: 720,
    };
    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains("\"camera\""), "wire deve levar \"camera\", deu: {json}");
    let back: SourceInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(info, back);
}

#[test]
fn capabilities_expose_camera_axis_with_honest_reason() {
    let caps: CapabilitySet = golive_platform::capabilities();
    assert!(
        !caps.camera.reason.is_empty(),
        "camera precisa de motivo honesto para a UI desabilitar com explicação"
    );
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    assert!(caps.camera.supported, "desktop real deve suportar webcam");
}

#[test]
fn camera_source_info_roundtrips_like_display() {
    for kind in [SourceKind::Display, SourceKind::Window, SourceKind::Camera] {
        let info = SourceInfo {
            kind,
            id: "7".to_owned(),
            name: "x".to_owned(),
            w: 640,
            h: 480,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: SourceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back, "roundtrip quebrou para {kind:?}");
    }
}

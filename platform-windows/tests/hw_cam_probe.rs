//! Real-hardware webcam probe (ignored by default: needs a physical camera).
//! Run serially (one device, single open):
//! `cargo test -p golive-platform-windows --test hw_cam_probe -- --ignored --nocapture --test-threads=1`
//!
//! Proves, against the user's live webcam: enumerate lists cameras,
//! open/start yields decoded CPU frames, thumbnail grabs one still.
//! Virtual devices with no fulfillable MF mode (e.g. Iriun) are skipped
//! honestly per test. Never prints names/pixels — counts and dims only.

use golive_platform::{CaptureConfig, CapturePacket, SourceInfo, SourceKind, VideoSource};
use golive_platform_windows::WindowsSource;
use std::time::Duration;

fn cameras() -> Vec<SourceInfo> {
    let list = WindowsSource::enumerate().expect("enumerate touches real OS");
    list.into_iter().filter(|s| s.kind == SourceKind::Camera).collect()
}

/// First camera that opens AND yields a frame (virtual devices with no
/// fulfillable MediaFoundation mode are skipped, never failed).
fn working_camera_info() -> SourceInfo {
    let cams = cameras();
    assert!(!cams.is_empty(), "expected a live webcam on this machine");
    for info in &cams {
        let open = WindowsSource::open(info);
        if open.is_err() {
            println!("camera id={} skipped at open", info.id);
            continue;
        }
        let mut source = open.unwrap();
        match source.start(&CaptureConfig { width: 640, height: 480, fps: 15 }) {
            Ok(mut stream) => {
                let first = stream.next_frame(Duration::from_secs(15));
                let _ = stream.stop(Duration::from_secs(2));
                match first {
                    Ok(CapturePacket::Cpu(_)) => {
                        println!("camera id={} works", info.id);
                        return info.clone();
                    }
                    other => println!("camera id={} skipped: {other:?}", info.id),
                }
            }
            Err(e) => println!("camera id={} skipped: {e}", info.id),
        }
    }
    panic!("no camera with a fulfillable mode");
}

#[test]
#[ignore]
fn hw_enumerate_lists_at_least_one_camera() {
    let cams = cameras();
    assert!(!cams.is_empty(), "expected a live webcam on this machine");
    for cam in &cams {
        assert!(!cam.id.trim().is_empty());
        println!("camera id={} {}x{}", cam.id, cam.w, cam.h);
    }
    println!("cameras: {}", cams.len());
}

#[test]
#[ignore]
fn hw_camera_streams_decoded_frames() {
    let info = working_camera_info();
    let mut source = WindowsSource::open(&info).expect("open working camera");
    let mut stream = source
        .start(&CaptureConfig { width: 1280, height: 720, fps: 15 })
        .expect("start camera stream");
    let mut decoded = 0u32;
    for _ in 0..5 {
        match stream.next_frame(Duration::from_secs(15)) {
            Ok(CapturePacket::Cpu(frame)) => {
                assert!(frame.w >= 2 && frame.h >= 2);
                assert_eq!(frame.data.len(), frame.stride * frame.h as usize);
                println!("frame {}x{} stride={}", frame.w, frame.h, frame.stride);
                decoded += 1;
            }
            Ok(CapturePacket::Gpu(_)) => {
                panic!("camera pump must emit CPU packets")
            }
            Err(e) => panic!("camera stream failed: {e:?}"),
        }
    }
    stream.stop(Duration::from_secs(2)).expect("bounded stop");
    assert_eq!(decoded, 5);
    println!("decoded: {decoded}");
}

#[test]
#[ignore]
fn hw_camera_thumbnail_grabs_one_still() {
    let info = working_camera_info();
    let still =
        golive_platform_windows::thumbnail(info.kind, &info.id).expect("one-shot thumbnail");
    assert!(still.w >= 2 && still.h >= 2 && !still.data.is_empty());
    println!("thumbnail {}x{}", still.w, still.h);
}

//! Smoke test: shell commands against the real local `server/`, no UI.
//!
//! Proves create_room → start_share(synthetic) → snapshot(Live) →
//! stop_share → snapshot(Stopped) → leave through the same `AppState`
//! methods the Tauri commands call.

use golive_app::AppState;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct ServerGuard {
    child: Child,
    base: String,
}

impl ServerGuard {
    fn spawn() -> Result<Self, String> {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let server = manifest.join("../server/server.mjs");
        if !server.exists() {
            return Err(format!("server not found: {}", server.display()));
        }
        let mut child = Command::new("node")
            .arg(&server)
            .env("PORT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn node: {e}"))?;
        macro_rules! bail {
            ($msg:expr) => {{
                let _ = child.kill();
                let _ = child.wait();
                return Err($msg.into());
            }};
        }
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => bail!("server stdout was not piped"),
        };
        // Drain stdout (discard): an unread pipe would wedge the server once
        // its kernel buffer fills. Port scrape reads lines directly below is
        // wrong — instead a background thread drains everything and we watch
        // a copy. Simplest correct: drain thread + scrape via temp file.
        let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
        let _ = std::thread::Builder::new()
            .name("smoke-server-log".into())
            .spawn(move || {                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                let mut sent = false;
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if !sent {
                                if let Some(p) = parse_port(&line) {
                                    let _ = port_tx.send(p);
                                    sent = true;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| format!("spawn log thread: {e}"));
        let port = match port_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(port) => port,
            Err(_) => bail!("server never printed a listen line"),
        };
        let base = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if Instant::now() > deadline {
                bail!("server health never turned green");
            }
            if let Ok(resp) = ureq::Agent::new_with_defaults()
                .get(&format!("{base}/health"))
                .call()
            {
                if resp.status().as_u16() == 200 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(Self { child, base })
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_port(line: &str) -> Option<u16> {
    let marker = "listen 127.0.0.1:";
    let start = line.find(marker)? + marker.len();
    line[start..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_create_share_snapshot() {
    let server = ServerGuard::spawn().expect("server");
    let state = Arc::new(AppState::new());

    // Point the shell at the ephemeral server.
    let base = state.set_server(&server.base).expect("set_server");
    assert_eq!(base, server.base);

    // Bad inputs are rejected without side effects.
    assert!(state.set_server("not-a-url").is_err());
    assert!(state.start_share(None, "bogus", None).await.is_err());
    assert!(state.start_share(None, "synthetic", None).await.is_err()); // not in a room

    // create → share → snapshot(Live with a share id).
    // With a plan installed, the room code is also published to code_file.
    let dir = std::env::temp_dir().join(format!("golive-smoke-e2e-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    state
        .set_e2e_plan(golive_app::E2ePlan {
            role: "host".into(),
            server: server.base.clone(),
            password: "smoke-password-123".into(),
            nickname: "smoke".into(),
            code_file: dir.join("code").to_string_lossy().into_owned(),
            status_file: dir.join("status.json").to_string_lossy().into_owned(),
            share: None,
            quality: None,
        })
        .expect("set_e2e_plan");
    let code = state
        .create_room(None, "smoke", "smoke-password-123")
        .await
        .expect("create_room");
    assert_eq!(code.len(), 6);
    assert_eq!(
        std::fs::read_to_string(dir.join("code")).expect("code file"),
        code
    );
    // Plan-gated helpers work with the plan, refuse nothing here.
    assert_eq!(state.e2e_read_code().expect("read code"), code);
    let _ = std::fs::remove_dir_all(&dir);
    state
        .start_share(None, "synthetic", None)
        .await
        .expect("start_share");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let snap = state.get_snapshot().expect("snapshot");
    assert_eq!(
        format!("{:?}", snap.share.state),
        "Live",
        "share is live"
    );
    assert!(snap.share.id.is_some(), "share has an id");

    // stop → snapshot(Stopped, id cleared) → leave. Double-stop is safe.
    state.stop_share().await.expect("stop_share");
    state.stop_share().await.expect("stop_share idempotent");
    let snap = state.get_snapshot().expect("snapshot");
    assert_eq!(format!("{:?}", snap.share.state), "Stopped");
    assert!(snap.share.id.is_none());
    state.leave().await.expect("leave");
    state.leave().await.expect("leave idempotent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_join_does_not_wedge_session() {
    let server = ServerGuard::spawn().expect("server");
    let host = Arc::new(AppState::new());
    host.set_server(&server.base).expect("set_server");
    let code = host
        .create_room(None, "host", "good-password-1")
        .await
        .expect("create_room");

    let guest = Arc::new(AppState::new());
    guest.set_server(&server.base).expect("set_server");
    assert!(
        guest
            .join_room(None, &code, "guest", "wrong-password")
            .await
            .is_err(),
        "wrong password must fail"
    );
    guest
        .join_room(None, &code, "guest", "good-password-1")
        .await
        .expect("retry after failed join must not be SessionBusy");
    guest.leave().await.expect("leave");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_watch_each_other_while_sharing() {
    run_mutual_watch(true).await;
    run_mutual_watch(false).await;
}

async fn run_mutual_watch(room_creator_shares_first: bool) {
    let server = ServerGuard::spawn().expect("server");
    let a = Arc::new(AppState::new());
    let b = Arc::new(AppState::new());
    a.set_server(&server.base).unwrap();
    b.set_server(&server.base).unwrap();
    let code = a
        .create_room(None, "alpha", "duplex-test-password")
        .await
        .unwrap();
    b.join_room(None, &code, "bravo", "duplex-test-password")
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let (a_id, b_id) = loop {
        let a_id = b
            .get_roster()
            .into_iter()
            .find(|m| m.nickname == "alpha")
            .map(|m| m.id);
        let b_id = a
            .get_roster()
            .into_iter()
            .find(|m| m.nickname == "bravo")
            .map(|m| m.id);
        if let (Some(a_id), Some(b_id)) = (a_id, b_id) {
            break (a_id, b_id);
        }
        assert!(Instant::now() < deadline, "rosters must arrive");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let (a, b, a_id, b_id) = if room_creator_shares_first {
        (a, b, a_id, b_id)
    } else {
        (b, a, b_id, a_id)
    };
    a.start_share(None, "synthetic", None).await.unwrap();
    b.watch(&a_id).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while b.get_media_counters().unwrap().frames < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let first = b.get_media_counters().unwrap().frames;
    if first < 3 {
        a.leave().await.unwrap();
        b.leave().await.unwrap();
        panic!("initial A -> B must deliver video: {first}");
    }
    b.start_share(None, "synthetic", None).await.unwrap();
    a.watch(&b_id).await.unwrap();
    let baseline = b.get_media_counters().unwrap().frames;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if a.get_media_counters().unwrap().frames >= 3
            && b.get_media_counters().unwrap().frames >= baseline + 3
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let reverse = a.get_media_counters().unwrap().frames;
    let forward = b
        .get_media_counters()
        .unwrap()
        .frames
        .saturating_sub(baseline);
    // Repeated UI intent must not leave an unowned viewer running after stop.
    b.watch(&a_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    b.unwatch(&a_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after_unwatch = b.get_media_counters().unwrap().frames;
    b.leave().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !a.get_snapshot().unwrap().links.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let orphan_links = a.get_snapshot().unwrap().links.len();
    a.leave().await.unwrap();
    assert_eq!(orphan_links, 0, "departed peer must not leave media links behind");
    assert_eq!(after_unwatch, 0, "duplicate watch must not leave ghost video after unwatch");
    assert!(
        reverse >= 3 && forward >= 3,
        "both directions must deliver video: A -> B fresh={forward}, B -> A={reverse}"
    );
}

async fn wait_for_frames(state: &Arc<AppState>, minimum: u64) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let frames = state.get_media_counters().unwrap().frames;
        if frames >= minimum { return; }
        assert!(Instant::now() < deadline, "fresh video stalled: {frames} < {minimum}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Real signaling + shell orchestration, including ownership transfer and
/// re-watch. Core-only fanout tests cannot catch shell lifecycle regressions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_host_survives_viewer_departure_quality_change_and_rewatch() {
    let server = ServerGuard::spawn().unwrap();
    let host = Arc::new(AppState::new());
    let first = Arc::new(AppState::new());
    let second = Arc::new(AppState::new());
    for state in [&host, &first, &second] { state.set_server(&server.base).unwrap(); }
    let code = host.create_room(None, "shared-host", "local-fanout-test").await.unwrap();
    first.join_room(None, &code, "first", "local-fanout-test").await.unwrap();
    second.join_room(None, &code, "second", "local-fanout-test").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let host_id = loop {
        if let Some(member) = second.get_roster().into_iter().find(|m| m.nickname == "shared-host") {
            break member.id;
        }
        assert!(Instant::now() < deadline, "host missing from roster");
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    host.start_share(None, "synthetic", Some(golive_core::media::QualityProfile {
        w: 320, h: 180, fps: 30, bitrate_kbps: 600,
    })).await.unwrap();
    first.watch(&host_id).await.unwrap();
    wait_for_frames(&first, 5).await;
    second.watch(&host_id).await.unwrap();
    wait_for_frames(&second, 5).await;

    first.leave().await.unwrap();
    let before = second.get_media_counters().unwrap().frames;
    host.set_quality(None, golive_app::SetQualityArgs {
        w: 640, h: 360, fps: 30, bitrate_kbps: 1200, preset: None,
    }).await.unwrap();
    wait_for_frames(&second, before + 15).await;

    // Last viewer out, then in: the encoder must be recreated and decode again.
    second.unwatch(&host_id).await.unwrap();
    assert_eq!(second.get_media_counters().unwrap().frames, 0);
    second.watch(&host_id).await.unwrap();
    wait_for_frames(&second, 5).await;
    second.leave().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !host.get_snapshot().unwrap().links.is_empty() {
        assert!(Instant::now() < deadline, "departed viewers left orphan links");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    host.stop_share().await.unwrap();
    host.leave().await.unwrap();
}

/// Native UI fixture: three real, independent senders in one local room.
/// Run explicitly with GOLIVE_PLAYER_FIXTURE_DIR set, join using room.json,
/// and create a `stop` file to shut everything down (ten-minute safety limit).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "interactive player fixture; requires GOLIVE_PLAYER_FIXTURE_DIR"]
async fn three_sender_player_fixture() {
    let dir = PathBuf::from(std::env::var("GOLIVE_PLAYER_FIXTURE_DIR").expect("fixture directory"));
    std::fs::create_dir_all(&dir).unwrap();
    let server = ServerGuard::spawn().unwrap();
    let first = Arc::new(AppState::new());
    first.set_server(&server.base).unwrap();
    let code = first.create_room(None, "Tela Um", "player-test").await.unwrap();
    let mut senders = vec![first];
    for name in ["Tela Dois", "Tela Tres"] {
        let sender = Arc::new(AppState::new());
        sender.set_server(&server.base).unwrap();
        sender.join_room(None, &code, name, "player-test").await.unwrap();
        senders.push(sender);
    }
    for sender in &senders {
        sender.start_share(None, "synthetic", Some(golive_core::media::QualityProfile { w: 640, h: 360, fps: 15, bitrate_kbps: 1200 })).await.unwrap();
    }
    std::fs::write(dir.join("room.json"), serde_json::to_vec(&serde_json::json!({"server": server.base, "code": code, "password": "player-test"})).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    while Instant::now() < deadline && !dir.join("stop").exists() { tokio::time::sleep(Duration::from_millis(200)).await; }
    for sender in senders { sender.leave().await.unwrap(); }
}

/// Modal preview × share contention on one physical webcam, then the stage
/// self-view on the live share. Locks the UI order the modal enforces
/// (stop preview → share → mirror), against a real room + real device.
///
/// - preview_start holds the camera (first GLP2 packet proves it);
/// - start_share on the held device either joins it (sharing drivers) or
///   fails typed (single-open drivers) — never wedges, never panics;
/// - after preview_stop the same share goes Live;
/// - selfview_start then mirrors I420 GLP2 frames (format field == 1).
/// Skips honestly with no webcam (CI/headless); virtual devices with no
/// fulfillable mode are skipped per camera, never failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn camera_preview_share_selfview_flow() {
    use golive_platform::SourceKind;
    use tauri::ipc::{Channel, InvokeResponseBody};
    let server = ServerGuard::spawn().expect("server");
    let state = Arc::new(AppState::new());
    // A device may open yet never deliver frames (virtual cameras) — only
    // received bytes prove a usable webcam. Tried in list order, skipped
    // honestly; the validated preview is stopped before sharing (UI order).
    let mut camera_id: Option<String> = None;
    for cam in state
        .list_sources()
        .await
        .expect("list_sources")
        .into_iter()
        .filter(|s| s.kind == SourceKind::Camera)
    {
        let (pv_tx, pv_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
        let preview = Channel::new(move |body| {
            if let InvokeResponseBody::Raw(bytes) = body {
                let _ = pv_tx.try_send(bytes);
            }
            Ok(())
        });
        match state.preview_start("camera", &cam.id, preview) {
            Ok(token) => match pv_rx.recv_timeout(Duration::from_secs(12)) {
                Ok(first) => {
                    assert!(first.len() > 20 && &first[..4] == b"GLP2");
                    state.preview_stop(&token);
                    camera_id = Some(cam.id.clone());
                    break;
                }
                Err(e) => {
                    eprintln!("preview camera without frames: {e}");
                    state.preview_stop(&token);
                }
            },
            Err(e) => eprintln!("preview skipping camera: {e}"),
        }
    }
    let Some(camera_id) = camera_id else {
        eprintln!("no previewable webcam on this machine; skipping");
        return;
    };
    // Reopen the preview to prove the share-ordering rule below from a
    // held device, like the modal does.
    let (pv_tx, pv_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    let preview = Channel::new(move |body| {
        if let InvokeResponseBody::Raw(bytes) = body {
            let _ = pv_tx.try_send(bytes);
        }
        Ok(())
    });
    let token = state
        .preview_start("camera", &camera_id, preview)
        .expect("preview opens the validated camera");
    let first = pv_rx.recv_timeout(Duration::from_secs(20)).expect("preview frame");
    assert!(first.len() > 20 && &first[..4] == b"GLP2");
    drop(pv_rx);

    state.set_server(&server.base).expect("set_server");
    state
        .create_room(None, "cam", "preview-contention-1")
        .await
        .expect("create_room");
    let share_desc = format!("camera:{camera_id}");
    match state.start_share(None, &share_desc, None).await {
        Ok(()) => state.preview_stop(&token),
        Err(e) => {
            assert!(!e.is_empty(), "contention must fail typed, never empty");
            eprintln!("single-open driver refused the held camera: {e}");
            state.preview_stop(&token);
            state
                .start_share(None, &share_desc, None)
                .await
                .expect("share after preview stop");
        }
    }
    let snap = state.get_snapshot().expect("snapshot");
    assert_eq!(format!("{:?}", snap.share.state), "Live");

    // Self-view mirrors the live share feed (no second device open).
    let (sv_tx, sv_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    let selfview = Channel::new(move |body| {
        if let InvokeResponseBody::Raw(bytes) = body {
            let _ = sv_tx.try_send(bytes);
        }
        Ok(())
    });
    let sv_token = state.selfview_start(selfview).expect("selfview on live share");
    let mut got = 0u32;
    let deadline = Instant::now() + Duration::from_secs(20);
    while got < 3 && Instant::now() < deadline {
        match sv_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(bytes) => {
                assert!(bytes.len() > 20 && &bytes[..4] == b"GLP2");
                assert_eq!(&bytes[16..20], &1u32.to_le_bytes(), "self-view is I420");
                got += 1;
            }
            Err(_) => break,
        }
    }
    state.selfview_stop(&sv_token);
    state.stop_share().await.expect("stop_share");
    state.leave().await.expect("leave");
    assert_eq!(got, 3, "self-view must mirror live share frames");
}

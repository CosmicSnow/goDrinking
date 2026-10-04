#!/usr/bin/env python3
"""Two-instance webcam validation with real binaries (Windows shaping).

Modes (each: host shares, viewer watches, both must show live video):
  1. combo   — screen + webcam corner overlay in one feed
  2. screen  — display only (regression)
  3. camera  — webcam only

Usage:
  python scripts/e2e-webcam.py --artifact e2e-artifacts/webcam-<ts>
  optional: --binary, --camera (default auto 1 then 0), --display,
            --timeout-s per mode, --quality 720p|1080p

Pass = every mode reaches host-connected + host-keyframe AND
viewer-connected + viewer-frames + viewer-presented (native acks).
Virtual cameras with no MF mode are skipped honestly per mode.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def read_json(path):
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return {}


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class ProcSet:
    def __init__(self, art):
        self.children = []
        self.logs = []
        self.art = art

    def launch(self, command, name, extra_env):
        log = (self.art / f"{name}.log").open("w", encoding="utf-8")
        self.logs.append(log)
        proc = subprocess.Popen([str(c) for c in command], cwd=ROOT,
                                env={**os.environ, **extra_env},
                                stdout=log, stderr=subprocess.STDOUT)
        self.children.append(proc)
        return proc

    def close(self):
        for child in reversed(self.children):
            if child.poll() is None:
                child.terminate()
        for child in reversed(self.children):
            try:
                child.wait(timeout=8)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        for log in self.logs:
            log.close()
        self.children = []
        self.logs = []


def wait_server(base, timeout=15):
    deadline = time.monotonic() + timeout
    while True:
        try:
            with urllib.request.urlopen(base + "/health", timeout=1):
                return True
        except OSError:
            if time.monotonic() > deadline:
                return False
            time.sleep(0.1)


def run_mode(binary, art, mode, share, quality, timeout, password="webcam-e2e-local"):
    """One host+viewer round. Returns (ok, detail dict)."""
    procs = ProcSet(art)
    detail = {"mode": mode, "share": share}
    try:
        port = free_port()
        base = f"http://127.0.0.1:{port}"
        procs.launch(["node", str(ROOT / "server/server.mjs")], f"{mode}-server",
                     {"PORT": str(port), "BIND": "127.0.0.1"})
        if not wait_server(base):
            return False, {**detail, "missing": "server-up"}
        host_plan = {"role": "host", "server": base, "password": password,
                     "nickname": f"host-{mode}", "code_file": str(art / f"{mode}-code"),
                     "status_file": str(art / f"{mode}-host.json"),
                     "share": share, "quality": quality}
        viewer_plan = {"role": "viewer", "server": base, "password": password,
                       "nickname": f"viewer-{mode}", "code_file": str(art / f"{mode}-code"),
                       "status_file": str(art / f"{mode}-viewer.json")}
        host_proc = procs.launch([str(binary), "--e2e-plan", json.dumps(host_plan)],
                                 f"{mode}-host", {"GOLIVE_TRACE_DIR": str(art / f"{mode}-host-trace")})
        # The guest joins with the code in hand in real usage — and two
        # Tauri/WebView boots at the same instant starve each other on this
        # machine (host never reaches create_room). Wait for the room code
        # before booting the viewer.
        verdict = None
        code_path = art / f"{mode}-code"
        code_deadline = time.monotonic() + 60
        while not code_path.exists():
            if host_proc.poll() is not None:
                verdict = "host-exited-before-room"
                break
            if time.monotonic() > code_deadline:
                verdict = "room-code-never-published"
                break
            time.sleep(0.5)
        else:
            viewer_proc = procs.launch([str(binary), "--e2e-plan", json.dumps(viewer_plan)],
                                       f"{mode}-viewer", {"GOLIVE_TRACE_DIR": str(art / f"{mode}-viewer-trace")})
        if verdict is not None:
            detail["verdict"] = verdict
            return verdict == "pass", detail
        deadline = time.monotonic() + timeout
        verdict = None
        latched_host = False
        latched_viewer = False
        while True:
            host = read_json(art / f"{mode}-host.json")
            viewer = read_json(art / f"{mode}-viewer.json")
            detail.update({f"host_{k}": host.get(k) for k in ("state", "connected", "keyframesSeen")}
                          | {f"viewer_{k}": viewer.get(k) for k in ("state", "connected", "frames", "presented")})
            if host_proc.poll() is not None or viewer_proc.poll() is not None:
                verdict = "process-exited"
                break
            # Latch: video that flowed counts even if a later optional step
            # (e.g. mid-share set_quality) reports an error afterwards.
            if host.get("connected") is True and host.get("keyframesSeen") is True:
                latched_host = True
            if (viewer.get("connected") is True
                    and isinstance(viewer.get("frames"), int) and viewer["frames"] > 0
                    and isinstance(viewer.get("presented"), int) and viewer["presented"] > 0):
                latched_viewer = True
            detail["latched_host"] = latched_host
            detail["latched_viewer"] = latched_viewer
            if latched_host and latched_viewer:
                verdict = "pass"
                break
            if time.monotonic() > deadline:
                if host.get("state") == "error":
                    verdict = f"host-error: {host.get('detail')}"
                elif viewer.get("state") == "error":
                    verdict = f"viewer-error: {viewer.get('detail')}"
                else:
                    missing = []
                    if not latched_host:
                        missing.append("host-connected+keyframe")
                    if not latched_viewer:
                        missing.append("viewer-connected+frames+presented")
                    verdict = "timeout:" + "+".join(missing)
                break
            time.sleep(0.5)
        detail["verdict"] = verdict
        return verdict == "pass", detail
    finally:
        procs.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--binary", type=Path,
                        default=ROOT / "app/target/release/goDrinking.exe")
    parser.add_argument("--camera", default="auto",
                        help='"auto" tries 1 then 0, else an explicit id')
    parser.add_argument("--display", default=r"\\.\DISPLAY1")
    parser.add_argument("--timeout-s", type=int, default=150)
    parser.add_argument("--quality", default="720p", choices=("720p", "1080p"))
    parser.add_argument("--only", nargs="*", choices=("combo", "screen", "camera"),
                        help="run only these modes (camera still needs its probe to resolve the id)")
    parser.add_argument("--settle-s", type=int, default=15,
                        help="quiet seconds between modes so MF/DXGI/WebView2 teardown "
                             "settles before the next host boots (a fresh boot right after "
                             "terminate() can stall pre-room)")
    args = parser.parse_args()
    art = args.artifact.resolve()
    art.mkdir(parents=True, exist_ok=False)
    if not args.binary.is_file():
        raise SystemExit(f"binary missing: {args.binary} (build release first)")
    camera_ids = ["1", "0"] if args.camera == "auto" else [args.camera]
    quality = {"w": 1280, "h": 720, "bitrate_kbps": 2000, "fps": 30} if args.quality == "720p" \
        else {"w": 1920, "h": 1080, "bitrate_kbps": 6000, "fps": 30}
    # Mode order per request: combo first, then screen, then camera-only.
    # Camera share is resolved per mode (first id that yields live video).
    results = []
    resolved_camera = None

    def resolve_camera():
        nonlocal resolved_camera
        if resolved_camera is not None:
            return resolved_camera
        for cid in camera_ids:
            ok, detail = run_mode(args.binary, art, f"probe-cam{cid}", f"camera:{cid}",
                                  quality, min(90, args.timeout_s))
            results.append(detail)
            if ok:
                resolved_camera = cid
                return cid
        return None

    modes = []
    # Combo needs a working camera id; resolve it first (also covers mode 3).
    cid = resolve_camera()
    if args.settle_s > 0:
        print(f"settling {args.settle_s}s after probe (driver/WebView teardown)...", flush=True)
        time.sleep(args.settle_s)
    if cid is None:
        modes.append(("combo", None))
        modes.append(("screen", f"display:{args.display}"))
        modes.append(("camera", None))
    else:
        modes.append(("combo", f"combo:display:{args.display}+camera:{cid}"))
        modes.append(("screen", f"display:{args.display}"))
        modes.append(("camera", f"camera:{cid}"))
    for mode, share in modes:
        if args.only and mode not in args.only:
            continue
        if share is None:
            results.append({"mode": mode, "share": None, "verdict": "skipped:no-working-camera"})
            continue
        if mode == "camera" and share == f"camera:{cid}":
            # Already proven by the probe round — record without re-running.
            probe = next(r for r in results if r["mode"] == f"probe-cam{cid}")
            results.append({"mode": mode, "share": share, "verdict": "pass",
                            "note": "proven by probe round", **{k: probe.get(k) for k in probe if k.startswith(("host_", "viewer_"))}})
            continue
        ok, detail = run_mode(args.binary, art, mode, share, quality, args.timeout_s)
        results.append(detail)
        if args.settle_s > 0:
            print(f"settling {args.settle_s}s (driver/WebView teardown)...", flush=True)
            time.sleep(args.settle_s)
    passed = all(r.get("verdict") == "pass" for r in results if not r["mode"].startswith("probe-"))
    # Probes that found nothing are informational; a probe that passed is evidence.
    report = {"passed": passed, "binary": str(args.binary), "quality": quality, "modes": results}
    (art / "verdict.json").write_text(json.dumps(report, indent=2) + "\n")
    for r in results:
        print(f"{'PASS' if r.get('verdict') == 'pass' else 'INFO' if r['mode'].startswith('probe-') else 'FAIL'} "
              f"{r['mode']}: {r.get('share')} -> {r.get('verdict')}")
    print("PASS" if passed else "FAIL")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())

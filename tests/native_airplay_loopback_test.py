#!/usr/bin/env python3
"""Native sender + DoubleTake receiver, isolated PipeWire and D-Bus only.

Set AIRPLAY_TEST_RECEIVER to the DoubleTake test-receiver executable. Builds
must already exist at engine/target/debug/omarchy-miracast. No desktop devices,
portals, TVs, or user credentials are touched.
"""
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
if len(sys.argv) == 1:
    subprocess.run(["dbus-run-session", "--", sys.executable, __file__, "--isolated"], check=True)
    raise SystemExit(0)

receiver_binary = os.environ["AIRPLAY_TEST_RECEIVER"]
engine_binary = ROOT / "engine/target/debug/omarchy-miracast"
temp_root = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None


def lines(stream):
    result = queue.Queue()
    def read():
        for line in stream:
            result.put(line)
    threading.Thread(target=read, daemon=True).start()
    return result


def until(q, predicate, seconds=20):
    deadline = time.monotonic() + seconds
    seen = []
    while time.monotonic() < deadline:
        try:
            line = q.get(timeout=max(0.01, deadline - time.monotonic()))
        except queue.Empty:
            break
        seen.append(line)
        if predicate(line):
            return line
    raise AssertionError("timeout; observed: " + "".join(seen[-30:]))


with tempfile.TemporaryDirectory(prefix="airplay-loopback-", dir=temp_root) as temp:
    root = Path(temp)
    runtime = root / "runtime"
    runtime.mkdir(mode=0o700)
    wp_config = root / "config/wireplumber/wireplumber.conf.d"
    wp_config.mkdir(parents=True)
    (wp_config / "99-no-hardware.conf").write_text(
        "wireplumber.profiles = { main = { monitor.alsa = disabled "
        "monitor.bluez = disabled monitor.bluez-midi = disabled } }\n")
    env = os.environ.copy()
    env.update(XDG_RUNTIME_DIR=str(runtime), PIPEWIRE_RUNTIME_DIR=str(runtime),
               XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"),
               DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(root / "no-system-bus"),
               PULSE_SERVER="unix:" + str(runtime / "pulse/native"),
               OMARCHY_AIRPLAY_BACKEND="native")
    processes = []
    try:
        def start(args, **kwargs):
            proc = subprocess.Popen(args, env=env, **kwargs)
            processes.append(proc)
            return proc
        start(["pipewire"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            if (runtime / "pipewire-0").exists(): break
            time.sleep(.05)
        else: raise RuntimeError("private PipeWire did not start")
        start(["pipewire-pulse"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        start(["wireplumber"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            if (runtime / "pulse/native").exists(): break
            time.sleep(.05)
        else: raise RuntimeError("private PipeWire Pulse server did not start")
        receiver = start([receiver_binary, "-listen", "127.0.0.1:0", "-profile", "lg",
                          "-auth", "pin", "-code", "1234", "-stats-interval", "100ms"],
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
        receiver_lines = lines(receiver.stdout)
        first = until(receiver_lines, lambda x: "127.0.0.1:" in x)
        address = re.search(r"127\.0\.0\.1:(\d+)", first)
        if not address: raise AssertionError(first)
        engine = start([engine_binary, "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                       stderr=subprocess.DEVNULL, text=True, bufsize=1)
        engine_lines = lines(engine.stdout)
        def event(line):
            try: return json.loads(line)
            except ValueError: return {}
        until(engine_lines, lambda line: event(line).get("event") == "ready")
        request = {"command":"connect", "protocol":"airplay", "ip":"127.0.0.1",
                   "port":int(address.group(1)), "mac":"02:00:00:00:00:01",
                   "name":"Loopback", "test":True, "display_mode":"mirror"}
        engine.stdin.write(json.dumps(request) + "\n")
        engine.stdin.flush()
        until(engine_lines, lambda line: event(line).get("event") == "pin_required")
        engine.stdin.write('{"command":"pin","pin":"1234"}\n')
        engine.stdin.flush()
        started = until(engine_lines, lambda line: event(line).get("phase") in ("streaming", "error"), 30)
        if event(started).get("phase") == "error": raise AssertionError(started)
        # Require actual encrypted audio and video packets, not just a state.
        def has_media(line):
            video = re.search(r"video=(\d+)", line)
            audio = re.search(r"audio=(\d+)", line)
            return bool(video and audio and int(video.group(1)) >= 2 and int(audio.group(1)) >= 2)
        until(receiver_lines, has_media, 15)
        engine.stdin.write('{"command":"stop"}\n')
        engine.stdin.flush()
        last = until(engine_lines, lambda line: event(line).get("phase") in ("idle", "error"), 10)
        if event(last).get("phase") == "error": raise AssertionError(last)
        # A second connection must verify the persisted pairing without a PIN.
        engine.stdin.write(json.dumps(request) + "\n")
        engine.stdin.flush()
        second = until(engine_lines, lambda line: event(line).get("event") == "pin_required" or
                       event(line).get("phase") in ("streaming", "error"), 30)
        if event(second).get("phase") != "streaming": raise AssertionError(second)
        engine.stdin.write('{"command":"stop"}\n')
        engine.stdin.flush()
        last = until(engine_lines, lambda line: event(line).get("phase") in ("idle", "error"), 10)
        if event(last).get("phase") == "error": raise AssertionError(last)
        engine.stdin.write('{"command":"quit"}\n')
        engine.stdin.flush()
        engine.wait(timeout=10)
        if engine.returncode: raise RuntimeError("engine failed")
        print("Native AirPlay: isolated PIN, audio, video, stop and credential resume passed")
    finally:
        for proc in reversed(processes):
            if proc.poll() is None: proc.terminate()
        for proc in reversed(processes):
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()

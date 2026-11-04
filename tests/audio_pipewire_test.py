#!/usr/bin/env python3
"""Exercise sink ownership against a private PipeWire, never the desktop graph."""
import os
from pathlib import Path
import select
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
TESTS = r'''
#[cfg(test)]
mod private_pipewire_lifecycle {
    use super::*;

    async fn gone(name: &str) {
        for _ in 0..50 {
            if !command_output("pw-dump", &[]).await.unwrap().contains(name) { return; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("owned sink was not removed");
    }

    #[tokio::test]
    async fn explicit_remove_and_drop_reap_owned_nodes() {
        let mut output = TvOutput::create().await.unwrap();
        assert!(output.node_id() > 0);
        let name = output.node_name().to_owned();
        output.remove().await;
        output.remove().await;
        gone(&name).await;
        let output = TvOutput::create().await.unwrap();
        let name = output.node_name().to_owned();
        drop(output);
        gone(&name).await;

        let output = TvOutput::create().await.unwrap();
        assert!(output.check().await.is_ok());
        unsafe { libc::kill(output.pid as i32, libc::SIGKILL); }
        for _ in 0..50 {
            if output.check().await.is_err() { break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(output.check().await.is_err(), "dead pw-cli reported healthy");
        let name = output.node_name().to_owned();
        drop(output);
        gone(&name).await;

        let output = TvOutput::create().await.unwrap();
        command_output("pw-cli", &["destroy", &output.node_id().to_string()]).await.unwrap();
        assert!(output.check().await.is_err(), "removed node reported healthy");
        drop(output);
    }
}
'''
MANIFEST = '''[package]
name = "screencast-private-pipewire-test"
version = "0.1.0"
edition = "2024"
[dependencies]
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
libc = "0.2"
tokio = { version = "1", features = ["rt", "time", "process", "macros", "sync"] }
'''

temp_root = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
with tempfile.TemporaryDirectory(prefix="screencast-pw-", dir=temp_root) as temp:
    root = Path(temp)
    runtime = root / "runtime"
    runtime.mkdir(mode=0o700)
    project = root / "project"
    (project / "src").mkdir(parents=True)
    (project / "Cargo.toml").write_text(MANIFEST)
    (project / "src/lib.rs").write_text((ROOT / "engine/src/audio.rs").read_text() + TESTS)
    (project / "src/main.rs").write_text('''
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let output = screencast_private_pipewire_test::TvOutput::create().await.unwrap();
    println!("{}", output.node_name());
    std::future::pending::<()>().await;
}
''')
    env = os.environ.copy()
    env.update(XDG_RUNTIME_DIR=str(runtime), PIPEWIRE_RUNTIME_DIR=str(runtime),
               XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"),
               DBUS_SESSION_BUS_ADDRESS="unix:path=" + str(root / "no-session-bus"))
    server = subprocess.Popen(["pipewire"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        for _ in range(100):
            if (runtime / "pipewire-0").exists():
                break
            if server.poll() is not None:
                raise RuntimeError(server.stderr.read().decode())
            time.sleep(0.05)
        else:
            raise RuntimeError("Private PipeWire did not start")
        subprocess.run(["cargo", "test", "--offline", "private_pipewire_lifecycle"],
                       cwd=project, env=env, check=True)
        subprocess.run(["cargo", "build", "--offline"], cwd=project, env=env, check=True)
        holder = subprocess.Popen([project / "target/debug/screencast-private-pipewire-test"],
                                  env=env, stdout=subprocess.PIPE, text=True)
        try:
            if not select.select([holder.stdout], [], [], 10)[0]:
                raise RuntimeError("Private sink startup timed out")
            name = holder.stdout.readline().strip()
            if not name.startswith("omarchy_airplay_"):
                raise RuntimeError("Private sink was not created")
            holder.kill()
            holder.wait(timeout=5)
            for _ in range(100):
                graph = subprocess.check_output(["pw-dump"], env=env, text=True)
                if name not in graph:
                    break
                time.sleep(0.05)
            else:
                raise RuntimeError("Sink survived SIGKILL of its owner")
        finally:
            if holder.poll() is None:
                holder.kill()
                holder.wait(timeout=5)
    finally:
        server.terminate()
        server.wait(timeout=5)

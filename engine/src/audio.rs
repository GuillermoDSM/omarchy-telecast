use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::timeout};

const SINK: &str = "omarchy_airplay";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
struct RouteState {
    previous: String,
}

/// Removes the temporary TV sink left by an older plugin version or an
/// interrupted session. The current AirPlay path captures the normal default
/// output, so it never creates this sink itself.
pub async fn cleanup_stale() {
    if !sink_exists().await && !state_path().exists() {
        return;
    }
    let previous = read_state().ok().map(|state| state.previous);
    let mut route = AudioRoute {
        previous,
        active: true,
    };
    route.restore().await;
}

pub async fn available() -> bool {
    command_output("pactl", &["info"]).await.is_ok()
}

struct AudioRoute {
    previous: Option<String>,
    active: bool,
}

impl AudioRoute {
    async fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;

        let current = command_output("pactl", &["get-default-sink"])
            .await
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let virtual_index = sink_index(SINK).await.ok();

        let mut target = if current.as_deref() == Some(SINK) {
            None
        } else {
            current.clone()
        };
        if target.is_none()
            && let Some(previous) = self.previous.as_deref()
            && sink_named_exists(previous).await
        {
            target = Some(previous.to_string());
        }
        if target.is_none() {
            target = fallback_sink().await;
        }

        if let Some(target) = target {
            if current.as_deref() == Some(SINK) {
                let _ = command_output("pactl", &["set-default-sink", &target]).await;
            }
            if let Some(index) = virtual_index {
                let _ = move_streams(Some(index), &target).await;
            }
        }

        destroy_sink().await;
        let _ = std::fs::remove_file(state_path());
    }
}

async fn destroy_sink() {
    let _ = command_output("pw-cli", &["destroy", SINK]).await;
}

async fn sink_exists() -> bool {
    sink_named_exists(SINK).await
}

async fn sink_named_exists(name: &str) -> bool {
    sink_index(name).await.is_ok()
}

async fn sink_index(name: &str) -> Result<u32> {
    let output = command_output("pactl", &["list", "short", "sinks"]).await?;
    for line in output.lines() {
        let mut fields = line.split('\t');
        let Some(index) = fields.next() else {
            continue;
        };
        let Some(sink) = fields.next() else {
            continue;
        };
        if sink == name {
            return index
                .parse()
                .with_context(|| format!("Índice de salida inválido: {index}"));
        }
    }
    bail!("No se encontró la salida {name}")
}

async fn fallback_sink() -> Option<String> {
    let output = command_output("pactl", &["list", "short", "sinks"])
        .await
        .ok()?;
    let mut first = None;
    for line in output.lines() {
        let mut fields = line.split('\t');
        fields.next();
        let Some(name) = fields.next() else {
            continue;
        };
        if name == SINK {
            continue;
        }
        if first.is_none() {
            first = Some(name.to_string());
        }
        if name.starts_with("alsa_output.") || name.starts_with("bluez_output.") {
            return Some(name.to_string());
        }
    }
    first
}

async fn move_streams(from: Option<u32>, to: &str) -> Result<()> {
    let output = command_output("pactl", &["list", "sink-inputs"]).await?;
    for id in parse_stream_ids(&output, from) {
        let _ = command_output("pactl", &["move-sink-input", &id, to]).await;
    }
    Ok(())
}

fn parse_stream_ids(output: &str, from: Option<u32>) -> Vec<String> {
    let mut result = Vec::new();
    let mut id: Option<String> = None;
    let mut sink: Option<u32> = None;
    let flush = |id: &Option<String>, sink: Option<u32>, result: &mut Vec<String>| {
        if let Some(id) = id
            && from.is_none_or(|expected| sink == Some(expected))
        {
            result.push(id.clone());
        }
    };
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("Sink Input #") {
            flush(&id, sink, &mut result);
            id = Some(value.trim().to_string());
            sink = None;
        } else if let Some(value) = line.trim_start().strip_prefix("Sink:") {
            sink = value.trim().parse().ok();
        }
    }
    flush(&id, sink, &mut result);
    result
}

async fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let mut command = Command::new(program);
    command.kill_on_drop(true);
    let output = timeout(
        COMMAND_TIMEOUT,
        command.args(args).stdin(Stdio::null()).output(),
    )
    .await
    .with_context(|| format!("{program} no respondió en 2 segundos"))?
    .with_context(|| format!("No se pudo ejecutar {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} terminó con {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn state_dir() -> Result<PathBuf> {
    let path = if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(value).join("omarchy-miracast")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/state/omarchy-miracast")
    } else {
        std::env::temp_dir().join(format!("omarchy-miracast-state-{}", unsafe {
            libc::getuid()
        }))
    };
    std::fs::create_dir_all(&path)
        .with_context(|| format!("No se pudo crear {}", path.display()))?;
    set_dir_mode(&path)?;
    Ok(path)
}

fn set_dir_mode(path: &Path) -> Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

fn state_path() -> PathBuf {
    state_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("audio-route.json")
}

fn read_state() -> Result<RouteState> {
    let body = std::fs::read(state_path())?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_matching_streams() {
        let output = "Sink Input #10\n\tSink: 5\n\tapplication.name = \"Chromium\"\nSink Input #11\n\tSink: 6\n\tapplication.name = \"mpv\"\nSink Input #12\n\tSink: 5\n";
        assert_eq!(parse_stream_ids(output, Some(5)), vec!["10", "12"]);
        assert_eq!(parse_stream_ids(output, None), vec!["10", "11", "12"]);
    }
}

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::Command,
    time::{sleep, timeout},
};

const SINK: &str = "omarchy_airplay";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const SINK_WAIT: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize)]
struct RouteState {
    previous: String,
}

/// A temporary PipeWire sink that represents the TV. It is created before the
/// AirPlay session starts and selected as the default output, so Omarchy's
/// normal audio panel can switch between the TV and the speakers like any
/// other output. DoubleTake captures this sink's monitor explicitly.
pub struct AudioRoute {
    previous: Option<String>,
    active: bool,
}

impl AudioRoute {
    pub async fn create(description: &str) -> Result<Self> {
        // A previous crash may have left the sink behind. It has no owner now.
        if sink_exists().await {
            destroy_sink().await;
        }
        let previous = command_output("pactl", &["get-default-sink"])
            .await
            .context("No se pudo consultar la salida de audio predeterminada")?
            .trim()
            .to_string();
        if previous.is_empty() {
            bail!("pactl no devolvió una salida de audio predeterminada");
        }
        if previous == SINK {
            bail!("La salida AirPlay ya estaba seleccionada; se limpiará al reintentar");
        }

        let volume = sink_volume_percent(&previous).await.ok();
        let muted = sink_mute(&previous).await.ok();

        create_sink(description).await?;
        wait_for_sink().await?;

        if let Some(volume) = volume {
            let value = format!("{volume}%");
            let _ = command_output("pactl", &["set-sink-volume", SINK, &value]).await;
        }
        if let Some(muted) = muted {
            let _ = command_output(
                "pactl",
                &["set-sink-mute", SINK, if muted { "1" } else { "0" }],
            )
            .await;
        }

        write_state(&RouteState {
            previous: previous.clone(),
        })?;
        Ok(Self {
            previous: Some(previous),
            active: true,
        })
    }

    /// Selects the TV output and moves current application streams to it.
    /// Called only when the AirPlay stream actually reaches PLAY, so pairing or
    /// a failed connection never steals the laptop's audio.
    pub async fn activate(&mut self) -> Result<()> {
        if !self.active {
            bail!("La salida AirPlay ya se cerró");
        }
        let current = command_output("pactl", &["get-default-sink"])
            .await
            .context("No se pudo consultar la salida de audio actual")?
            .trim()
            .to_string();
        if current == SINK {
            return Ok(());
        }
        if current.is_empty() {
            bail!("No hay una salida de audio que sustituir");
        }
        let volume = sink_volume_percent(&current).await.ok();
        let muted = sink_mute(&current).await.ok();
        let index = sink_index(&current).await.ok();

        self.previous = Some(current.clone());
        if let Some(volume) = volume {
            let value = format!("{volume}%");
            let _ = command_output("pactl", &["set-sink-volume", SINK, &value]).await;
        }
        if let Some(muted) = muted {
            let _ = command_output(
                "pactl",
                &["set-sink-mute", SINK, if muted { "1" } else { "0" }],
            )
            .await;
        }
        command_output("pactl", &["set-default-sink", SINK])
            .await
            .context("No se pudo seleccionar la salida AirPlay")?;
        if let Some(index) = index {
            let _ = move_streams(Some(index), SINK, true).await;
        }
        write_state(&RouteState { previous: current })?;
        Ok(())
    }

    pub async fn restore(&mut self) {
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
                let _ = move_streams(Some(index), &target, false).await;
            }
        }

        destroy_sink().await;
        let _ = std::fs::remove_file(state_path());
    }
}

/// Removes a sink left by an interrupted session. Called at engine start when
/// no AirPlay stream is active. If the previous output no longer exists, streams
/// are moved to the first available physical sink.
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

pub async fn tv_selected() -> bool {
    command_output("pactl", &["get-default-sink"])
        .await
        .is_ok_and(|sink| sink.trim() == SINK)
}

async fn create_sink(description: &str) -> Result<()> {
    let description = sanitize_description(description);
    let config = format!(
        "{{ factory.name=support.null-audio-sink node.name={SINK} node.description=\"{description}\" node.nick=\"{description}\" media.class=Audio/Sink object.linger=true audio.position=[FL FR] monitor.channel-volumes=true node.always-process=true node.pause-on-idle=false device.icon-name=video-display }}"
    );
    command_output("pw-cli", &["create-node", "adapter", &config])
        .await
        .context("No se pudo crear la salida de audio AirPlay")
        .map(|_| ())
}

async fn wait_for_sink() -> Result<()> {
    for _ in 0..(SINK_WAIT.as_millis() / 100) {
        if sink_exists().await {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    bail!("La salida de audio AirPlay no apareció en PipeWire")
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

async fn sink_volume_percent(sink: &str) -> Result<u32> {
    let output = command_output("pactl", &["get-sink-volume", sink]).await?;
    parse_volume_percent(&output).context("pactl no devolvió un volumen válido")
}

async fn sink_mute(sink: &str) -> Result<bool> {
    let output = command_output("pactl", &["get-sink-mute", sink]).await?;
    parse_mute(&output).with_context(|| format!("Respuesta inesperada de pactl: {output:?}"))
}

async fn move_streams(from: Option<u32>, to: &str, require_app: bool) -> Result<()> {
    let output = command_output("pactl", &["list", "sink-inputs"]).await?;
    let ids = parse_stream_ids(&output, from, require_app);
    for id in ids {
        let _ = command_output("pactl", &["move-sink-input", &id, to]).await;
    }
    Ok(())
}

fn parse_stream_ids(output: &str, from: Option<u32>, require_app: bool) -> Vec<String> {
    let mut result = Vec::new();
    let mut id: Option<String> = None;
    let mut sink: Option<u32> = None;
    let mut app = false;
    let flush = |id: &Option<String>, sink: Option<u32>, app: bool, result: &mut Vec<String>| {
        if let Some(id) = id
            && from.is_none_or(|expected| sink == Some(expected))
            && (!require_app || app)
        {
            result.push(id.clone());
        }
    };
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("Sink Input #") {
            flush(&id, sink, app, &mut result);
            id = Some(value.trim().to_string());
            sink = None;
            app = false;
        } else if let Some(value) = line.trim_start().strip_prefix("Sink:") {
            sink = value.trim().parse().ok();
        } else if line.trim_start().starts_with("application.name = ") {
            app = true;
        }
    }
    flush(&id, sink, app, &mut result);
    result
}

fn parse_volume_percent(output: &str) -> Option<u32> {
    let percent = output.find('%')?;
    let digits: String = output[..percent]
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

fn parse_mute(output: &str) -> Result<bool> {
    match output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Mute:"))
        .map(str::trim)
    {
        Some("yes") => Ok(true),
        Some("no") => Ok(false),
        _ => bail!("estado de silencio ausente"),
    }
}

fn sanitize_description(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '"' | '\\' | '{' | '}') {
                ' '
            } else {
                character
            }
        })
        .take(120)
        .collect()
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

fn write_state(state: &RouteState) -> Result<()> {
    let path = state_path();
    let body = serde_json::to_vec(state)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("No se pudo escribir {}", path.display()))?;
    file.write_all(&body)?;
    Ok(())
}

fn read_state() -> Result<RouteState> {
    let body = std::fs::read(state_path())?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pactl_volume_and_mute() {
        assert_eq!(
            parse_volume_percent(
                "Volume: front-left: 39321 /  60% / -13.31 dB, front-right: 1 / 0%"
            ),
            Some(60)
        );
        assert_eq!(parse_volume_percent("sin volumen"), None);
        assert!(parse_mute("Mute: yes\n").unwrap());
        assert!(!parse_mute("Mute: no\n").unwrap());
        assert!(parse_mute("Volume: 50%\n").is_err());
    }

    #[test]
    fn parses_only_matching_streams() {
        let output = "Sink Input #10\n\tSink: 5\n\tapplication.name = \"Chromium\"\nSink Input #11\n\tSink: 6\n\tapplication.name = \"mpv\"\nSink Input #12\n\tSink: 5\n";
        assert_eq!(parse_stream_ids(output, Some(5), true), vec!["10"]);
        assert_eq!(parse_stream_ids(output, Some(5), false), vec!["10", "12"]);
        assert_eq!(parse_stream_ids(output, None, true), vec!["10", "11"]);
    }

    #[test]
    fn sanitizes_sink_description() {
        assert_eq!(
            sanitize_description("Samsung \"TV\"\n{AirPlay}"),
            "Samsung  TV   AirPlay "
        );
    }
}

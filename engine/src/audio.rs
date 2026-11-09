use anyhow::{Context, Result, bail};
use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{process::Command, time::timeout};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn available() -> bool {
    command_output("pactl", &["info"]).await.is_ok()
}

pub async fn native_available() -> bool {
    command_output("pw-cli", &["--version"]).await.is_ok()
        && command_output("pw-dump", &[]).await.is_ok()
        && available().await
}

/// A native PipeWire sink, owned by a pw-cli client for its entire lifetime.
/// It does not select a default or move any streams. The consumer must keep
/// this value alive until it has stopped capturing the monitor ports.
pub struct TvOutput {
    name: String,
    node: u32,
    pid: u32,
    alive: Arc<AtomicBool>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    finished: Option<tokio::sync::oneshot::Receiver<()>>,
}

struct AliveGuard(Arc<AtomicBool>);
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn node_id(graph: &str, name: &str, pid: u32) -> Option<u32> {
    let objects: Vec<serde_json::Value> = serde_json::from_str(graph).ok()?;
    let client = objects.iter().find(|object| {
        object["type"] == "PipeWire:Interface:Client"
            && object["info"]["props"]["application.process.id"].as_u64() == Some(pid as u64)
    })?["id"]
        .as_u64()?;
    let mut matches = objects.iter().filter(|object| {
        object["type"] == "PipeWire:Interface:Node"
            && object["info"]["props"]["node.name"] == name
            && object["info"]["props"]["media.class"] == "Audio/Sink"
            && object["info"]["props"]["client.id"].as_u64() == Some(client)
    });
    let id = matches.next()?["id"].as_u64()?;
    if matches.next().is_some() {
        return None;
    }
    u32::try_from(id).ok()
}

impl TvOutput {
    pub async fn create() -> Result<Self> {
        let name = format!("omarchy_airplay_{}", std::process::id());
        let existing = command_output("pw-dump", &[]).await?;
        if existing.contains(&format!("\"{name}\"")) {
            bail!("La salida {name} ya existe; no se tomará control de ella");
        }
        let parent = unsafe { libc::getpid() };
        let (stop, stop_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, finished) = tokio::sync::oneshot::channel();
        let alive = Arc::new(AtomicBool::new(false));
        let child_alive = alive.clone();
        let child_name = name.clone();
        // Linux PDEATHSIG is tied to the *creating thread*, not the process.
        // Keep this dedicated supervisor thread alive until pw-cli is reaped.
        std::thread::spawn(move || {
            use std::{io::Write, os::unix::process::CommandExt};
            let _guard = AliveGuard(child_alive.clone());
            let mut command = std::process::Command::new("pw-cli");
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        return Err(std::io::Error::other("El motor ya terminó"));
                    }
                    Ok(())
                });
            }
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let _ = started_tx.send(Err(error));
                    return;
                }
            };
            let create = format!(
                "create-node adapter {{ factory.name=support.null-audio-sink node.name={child_name} node.description=Omarchy-AirPlay-TV media.class=Audio/Sink object.linger=false audio.position=[FL,FR] priority.session=1 }}\n"
            );
            let result = child
                .stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("pw-cli no tiene entrada"))
                .and_then(|stdin| stdin.write_all(create.as_bytes()))
                .map(|_| child.id());
            child_alive.store(result.is_ok(), Ordering::Release);
            if started_tx.send(result).is_ok() {
                loop {
                    match stop_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if !matches!(child.try_wait(), Ok(None)) {
                        break;
                    }
                }
            }
            child_alive.store(false, Ordering::Release);
            let _ = child.kill();
            let _ = child.wait();
            let _ = finished_tx.send(());
        });
        // This guard is created before the first await, so cancellation at any
        // point closes the channel and causes the supervisor to reap the child.
        let mut output = Self {
            name,
            node: 0,
            pid: 0,
            alive,
            stop: Some(stop),
            finished: Some(finished),
        };
        let pid = started_rx
            .await
            .context("Terminó el supervisor PipeWire")?
            .context("No se pudo iniciar pw-cli")?;
        output.pid = pid;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        loop {
            if !output.alive.load(Ordering::Acquire) {
                bail!("pw-cli terminó antes de publicar la salida");
            }
            if let Ok(graph) = command_output("pw-dump", &[]).await
                && let Some(node) = node_id(&graph, &output.name, pid)
            {
                output.node = node;
                output.check().await?;
                return Ok(output);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("PipeWire no publicó la salida de audio para la TV");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn node_name(&self) -> &str {
        &self.name
    }
    /// Used by tests/audio_pipewire_test.py, which injects its own test module.
    #[allow(dead_code)]
    #[cfg(test)]
    pub fn node_id(&self) -> u32 {
        self.node
    }
    /// PulseAudio compatibility name for the session TV sink.
    pub fn monitor(&self) -> String {
        format!("{}.monitor", self.name)
    }
    /// Fail closed if the owning client exited, or if the global id was
    /// removed/reused. Never reconnect capture to a replacement node.
    pub async fn check(&self) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            bail!("Terminó el cliente PipeWire de la salida TV");
        }
        let graph = command_output("pw-dump", &[]).await?;
        if node_id(&graph, &self.name, self.pid) != Some(self.node)
            || !self.alive.load(Ordering::Acquire)
        {
            bail!("La salida PipeWire de la TV ya no está disponible");
        }
        Ok(())
    }

    pub async fn wait_pulse_monitor(&self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        loop {
            self.check().await?;
            if self.pulse_monitor_exists().await? {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("PipeWire no publicó el monitor de la salida TV en PulseAudio");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn pulse_monitor_exists(&self) -> Result<bool> {
        let expected = self.monitor();
        let sources = command_output("pactl", &["list", "short", "sources"]).await?;
        Ok(sources
            .lines()
            .any(|line| line.split('\t').nth(1) == Some(expected.as_str())))
    }

    pub async fn remove(&mut self) {
        self.alive.store(false, Ordering::Release);
        self.stop.take();
        if let Some(finished) = self.finished.take() {
            let _ = finished.await;
        }
    }
}

/// Make the session's TV sink the default output and move streams that were on
/// the previous default. Returns the previous sink for restoration. Streams
/// explicitly routed elsewhere are left untouched.
pub async fn route_to_tv(tv: &str) -> Option<String> {
    let previous = command_output("pactl", &["get-default-sink"])
        .await
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    if previous == tv || !sink_named_exists(tv).await {
        return Some(previous);
    }
    let _ = command_output("pactl", &["set-default-sink", tv]).await;
    if let Ok(index) = sink_index(&previous).await {
        let _ = move_streams(Some(index), tv).await;
    }
    Some(previous)
}

/// Restore the saved default only if the session still owns the default sink.
/// A user choice made during the session always wins.
pub async fn restore_from_tv(tv: &str, previous: Option<&str>) {
    let current = command_output("pactl", &["get-default-sink"])
        .await
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let tv_index = sink_index(tv).await.ok();
    let mut target = if current.as_deref() == Some(tv) {
        None
    } else {
        current.clone()
    };
    if target.is_none()
        && let Some(previous) = previous
        && sink_named_exists(previous).await
    {
        target = Some(previous.to_string());
    }
    if target.is_none() {
        target = fallback_sink().await;
    }
    if let Some(target) = target {
        if current.as_deref() == Some(tv) {
            let _ = command_output("pactl", &["set-default-sink", &target]).await;
        }
        if let Some(index) = tv_index {
            let _ = move_streams(Some(index), &target).await;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_matching_streams() {
        let output = "Sink Input #10\n\tSink: 5\n\tapplication.name = \"Chromium\"\nSink Input #11\n\tSink: 6\n\tapplication.name = \"mpv\"\nSink Input #12\n\tSink: 5\n";
        assert_eq!(parse_stream_ids(output, Some(5)), vec!["10", "12"]);
        assert_eq!(parse_stream_ids(output, None), vec!["10", "11", "12"]);
    }

    #[test]
    fn node_lookup_requires_owner_and_exact_unambiguous_sink() {
        let graph = r#"[{"id":7,"type":"PipeWire:Interface:Client","info":{"props":{"application.process.id":123}}},{"id":42,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"tv","media.class":"Audio/Sink","client.id":7}}}]"#;
        assert_eq!(node_id(graph, "tv", 123), Some(42));
        assert_eq!(node_id(graph, "tv", 124), None);
        assert_eq!(node_id(graph, "t", 123), None);
        assert_eq!(node_id("invalid", "tv", 123), None);
        let duplicate = graph.replace("]", ", {\"id\":43,\"type\":\"PipeWire:Interface:Node\",\"info\":{\"props\":{\"node.name\":\"tv\",\"media.class\":\"Audio/Sink\",\"client.id\":7}}}]");
        assert_eq!(node_id(&duplicate, "tv", 123), None);
    }
}

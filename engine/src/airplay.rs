use crate::{Events, audio};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
    sync::Mutex,
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

const SOCKET_WAIT: Duration = Duration::from_secs(8);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const SCAN_WAIT: Duration = Duration::from_secs(12);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

struct Running {
    child: Child,
    test: bool,
}

/// Owns the DoubleTake daemon used for AirPlay mirroring. The daemon is kept
/// alive between scans so the mDNS cache and pairing state survive popup
/// reopens; it is stopped when the engine exits.
pub struct Manager {
    socket: PathBuf,
    creds: PathBuf,
    log: PathBuf,
    running: Mutex<Option<Running>>,
}

impl Manager {
    pub fn new() -> Result<Self> {
        let runtime = runtime_dir()?;
        let state = state_dir()?;
        std::fs::create_dir_all(&runtime)
            .with_context(|| format!("No se pudo crear {}", runtime.display()))?;
        std::fs::create_dir_all(&state)
            .with_context(|| format!("No se pudo crear {}", state.display()))?;
        set_dir_mode(&runtime)?;
        set_dir_mode(&state)?;
        Ok(Self {
            socket: runtime.join("airplay.sock"),
            creds: state.join("airplay-credentials.json"),
            log: state.join("airplay.log"),
            running: Mutex::new(None),
        })
    }

    pub async fn warm(&self) {
        let _ = self.ensure(false).await;
    }

    pub async fn is_streaming(&self) -> bool {
        if !self.socket.exists() {
            return false;
        }
        self.status()
            .await
            .is_ok_and(|status| status["state"] == "streaming")
    }

    pub async fn scan(&self) -> Result<Vec<Value>> {
        self.ensure(false).await?;
        let deadline = Instant::now() + SCAN_WAIT;
        let mut devices;
        loop {
            let response = self
                .request(json!({"cmd":"devices"}), REQUEST_TIMEOUT)
                .await?;
            devices = map_devices(&response);
            if !devices.is_empty() || Instant::now() >= deadline {
                break;
            }
            sleep(Duration::from_millis(250)).await;
        }
        Ok(devices)
    }

    pub async fn pin(&self, pin: &str) -> Result<()> {
        let response = self
            .request(json!({"cmd":"connect","pin":pin}), REQUEST_TIMEOUT)
            .await?;
        check(response).map(|_| ())
    }

    async fn status(&self) -> Result<Value> {
        self.request(json!({"cmd":"status"}), REQUEST_TIMEOUT).await
    }

    async fn connect(&self, target: &str, port: u16) -> Result<Value> {
        let response = self
            .request(
                json!({"cmd":"connect","target":target,"port":port}),
                REQUEST_TIMEOUT,
            )
            .await?;
        check(response)
    }

    async fn disconnect(&self) -> Result<()> {
        self.request(json!({"cmd":"disconnect"}), REQUEST_TIMEOUT)
            .await
            .map(|_| ())
    }

    async fn ensure(&self, test: bool) -> Result<()> {
        let mut guard = self.running.lock().await;
        if let Some(running) = guard.as_mut() {
            if running.test == test && running.child.try_wait()?.is_none() {
                if self
                    .request(json!({"cmd":"status"}), Duration::from_secs(1))
                    .await
                    .is_ok()
                {
                    return Ok(());
                }
            }
            stop_child(&mut running.child).await;
            *guard = None;
        }

        let binary = binary_path()?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.log)
            .with_context(|| format!("No se pudo abrir {}", self.log.display()))?;
        let mut command = Command::new(&binary);
        command
            .arg("-daemonize")
            .arg("-socket")
            .arg(&self.socket)
            .arg("-creds")
            .arg(&self.creds)
            .arg("-preserve-receiver-volume")
            .arg("-fps")
            .arg("30")
            .arg("-bitrate")
            .arg("0")
            .arg("-hwaccel")
            .arg("auto")
            .arg("-video-codec")
            .arg("auto")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        if std::env::var_os("OMARCHY_MIRACAST_AIRPLAY_DEBUG").is_some() {
            command.arg("-debug");
        }
        if test {
            command.arg("-test");
        }
        let child = command
            .spawn()
            .with_context(|| format!("No se pudo iniciar {}", binary.display()))?;
        *guard = Some(Running { child, test });

        let deadline = Instant::now() + SOCKET_WAIT;
        loop {
            match self.status().await {
                Ok(_) => return Ok(()),
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    sleep(Duration::from_millis(150)).await;
                }
                Err(error) => {
                    if let Some(mut running) = guard.take() {
                        stop_child(&mut running.child).await;
                    }
                    bail!(
                        "El emisor AirPlay no respondió en {}: {error:#}. Registro: {}",
                        SOCKET_WAIT.as_secs(),
                        self.log.display()
                    );
                }
            }
        }
    }

    async fn request(&self, request: Value, wait: Duration) -> Result<Value> {
        let mut stream = UnixStream::connect(&self.socket).await.with_context(|| {
            format!(
                "No se pudo conectar al emisor AirPlay en {}",
                self.socket.display()
            )
        })?;
        let mut body = serde_json::to_vec(&request)?;
        body.push(b'\n');
        stream.write_all(&body).await?;
        stream.flush().await?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let read = timeout(wait, reader.read_line(&mut line))
            .await
            .context("El emisor AirPlay no respondió a tiempo")??;
        if read == 0 {
            bail!("El emisor AirPlay cerró la conexión sin responder");
        }
        if line.len() > 64 * 1024 {
            bail!("Respuesta demasiado grande del emisor AirPlay");
        }
        Ok(serde_json::from_str(&line).context("Respuesta inválida del emisor AirPlay")?)
    }

    pub async fn shutdown(&self) {
        let _ = self.disconnect().await;
        let mut guard = self.running.lock().await;
        if let Some(mut running) = guard.take() {
            stop_child(&mut running.child).await;
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.running.try_lock()
            && let Some(running) = guard.as_mut()
        {
            let _ = running.child.start_kill();
        }
    }
}

async fn check_route(target: &str) -> Result<()> {
    let output = match Command::new("ip")
        .args(["route", "get", target])
        .stdin(Stdio::null())
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => return Ok(()),
    };
    if route_uses_tailscale(&output) {
        bail!(
            "Tailscale está desviando el tráfico hacia {target}. Desactiva Tailscale o añade una excepción temporal para la TV; el plugin no modifica rutas por su cuenta"
        );
    }
    Ok(())
}

fn route_uses_tailscale(output: &str) -> bool {
    output
        .split_whitespace()
        .any(|word| word == "tailscale0" || word.starts_with("tailscale"))
}

fn check(response: Value) -> Result<Value> {
    if response["ok"].as_bool().unwrap_or(false) {
        Ok(response)
    } else {
        bail!(
            "{}",
            response["error"]
                .as_str()
                .filter(|error| !error.is_empty())
                .unwrap_or("el emisor AirPlay rechazó la operación")
        )
    }
}

fn map_devices(response: &Value) -> Vec<Value> {
    let Some(devices) = response["devices"].as_array() else {
        return Vec::new();
    };
    devices
        .iter()
        .filter_map(|device| {
            let ip = clean(device["ip"].as_str().unwrap_or_default(), 64);
            let port = device["port"].as_u64().unwrap_or(0);
            let id = clean(device["device_id"].as_str().unwrap_or_default(), 64);
            if ip.is_empty() || port == 0 || port > u16::MAX as u64 {
                return None;
            }
            let name = clean(device["name"].as_str().unwrap_or_default(), 120);
            let model = clean(device["model"].as_str().unwrap_or_default(), 120);
            let mac = if id.is_empty() {
                ip.clone()
            } else {
                id.to_ascii_uppercase()
            };
            Some(json!({
                "protocol": "airplay",
                "mac": mac,
                "name": if name.is_empty() { ip.clone() } else { name },
                "model": model,
                "strength": 0,
                "wfd": false,
                "ip": ip,
                "port": port,
            }))
        })
        .collect()
}

pub async fn run(
    manager: std::sync::Arc<Manager>,
    target: String,
    port: u16,
    test: bool,
    name: String,
    events: Events,
    cancel: CancellationToken,
) {
    let name = clean(&name, 120);
    let mut last_state = String::new();
    let mut audio_warned = false;
    let result = async {
        manager.ensure(test).await?;
        check_route(&target).await?;
        events.state(
            "connecting",
            &format!("Conectando con {name} por AirPlay; acepta la solicitud en la TV"),
        );
        manager.connect(&target, port).await?;
        loop {
            if cancel.is_cancelled() {
                break;
            }
            let status = manager.status().await?;
            let state = status["state"].as_str().unwrap_or("idle");
            if state != last_state {
                match state {
                    "pin_required" => {
                        let kind = status["credential_kind"].as_str().unwrap_or("pin");
                        events.send(json!({
                            "event": "pin_required",
                            "kind": kind,
                            "device": name.clone(),
                        }));
                        events.state(
                            "pairing",
                            &format!(
                                "Introduce {} que muestra {name}",
                                if kind == "password" { "la contraseña" } else { "el PIN" }
                            ),
                        );
                    }
                    "streaming" => {
                        let has_audio = status["has_audio"].as_bool().unwrap_or(false);
                        events.send(json!({
                            "event": "airplay",
                            "device": name.clone(),
                            "audio": has_audio,
                        }));
                        let detail = if has_audio {
                            format!(
                                "AirPlay activo · audio y vídeo en {name}; el audio también suena en la laptop"
                            )
                        } else {
                            format!("AirPlay activo · vídeo sin audio en {name}")
                        };
                        events.state("streaming", &detail);
                        if !has_audio && !audio_warned {
                            audio_warned = true;
                            events.warning(
                                "La TV no negoció audio; la sesión AirPlay continuará solo con vídeo",
                            );
                        }
                    }
                    "idle" => {
                        events.state("idle", "Sesión AirPlay terminada");
                        break;
                    }
                    other => {
                        let error = status["error"].as_str().unwrap_or_default();
                        let detail = if error.is_empty() {
                            format!("AirPlay: {other}")
                        } else {
                            error.to_string()
                        };
                        events.state("connecting", &detail);
                    }
                }
                last_state = state.to_string();
            }
            if state == "idle" {
                break;
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = sleep(POLL_INTERVAL) => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = manager.disconnect().await;
    match result {
        Ok(()) => events.state("idle", "Sesión AirPlay terminada"),
        Err(error) => events.state("error", &format!("{error:#}")),
    }
}

pub async fn doctor() -> Vec<Value> {
    let mut checks = Vec::new();
    match binary_path() {
        Ok(path) => checks.push(json!({
            "name": "Emisor AirPlay (DoubleTake)",
            "ok": true,
            "detail": path.display().to_string(),
            "group": "airplay",
        })),
        Err(error) => checks.push(json!({
            "name": "Emisor AirPlay (DoubleTake)",
            "ok": false,
            "detail": format!("{error:#}"),
            "group": "airplay",
        })),
    }
    let openh264 = gstreamer::ElementFactory::find("openh264enc").is_some();
    checks.push(json!({
        "name": "Codificador AirPlay H.264",
        "ok": openh264,
        "detail": if openh264 { "openh264enc disponible" } else { "Falta openh264enc (gst-plugins-bad)" },
        "group": "airplay",
    }));
    let pactl = audio::available().await;
    checks.push(json!({
        "name": "Audio AirPlay",
        "ok": pactl,
        "detail": if pactl { "pactl disponible; la TV aparecerá como salida de audio temporal" } else { "Falta pactl (libpulse)" },
        "group": "airplay",
    }));
    checks
}

pub fn binary_path() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("OMARCHY_AIRPLAY_BIN") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "OMARCHY_AIRPLAY_BIN apunta a un archivo inexistente: {}",
            path.display()
        );
    }
    let executable = std::env::current_exe().context("No se pudo localizar el motor")?;
    if let Some(directory) = executable.parent() {
        let candidate = directory.join("doubletake");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    for candidate in ["/usr/bin/doubletake", "/usr/local/bin/doubletake"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    bail!("No se encontró el emisor AirPlay. Instala DoubleTake o define OMARCHY_AIRPLAY_BIN")
}

fn runtime_dir() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(value).join("omarchy-miracast"));
    }
    Ok(std::env::temp_dir().join(format!("omarchy-miracast-{}", unsafe { libc::getuid() })))
}

fn state_dir() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(value).join("omarchy-miracast"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local/state/omarchy-miracast"));
    }
    Ok(
        std::env::temp_dir().join(format!("omarchy-miracast-state-{}", unsafe {
            libc::getuid()
        })),
    )
}

fn set_dir_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

async fn stop_child(child: &mut Child) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    if timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

fn clean(value: &str, limit: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_airplay_devices_and_rejects_incomplete_entries() {
        let devices = map_devices(&json!({
            "devices": [
                {"name":"Samsung 7 Series (50)","model":"UTU7000","ip":"192.168.1.139","port":59757,"device_id":"68:72:C3:A8:40:DF"},
                {"name":"sin IP","ip":"","port":59757,"device_id":"00:00:00:00:00:01"},
                {"name":"sin puerto","ip":"192.168.1.2","port":0,"device_id":"00:00:00:00:00:02"}
            ]
        }));
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0]["protocol"], "airplay");
        assert_eq!(devices[0]["mac"], "68:72:C3:A8:40:DF");
        assert_eq!(devices[0]["port"], 59757);
        assert_eq!(devices[0]["wfd"], false);
    }

    #[test]
    fn rejects_failed_daemon_operations() {
        let error = check(json!({"ok":false,"error":"receiver rejected pairing"})).unwrap_err();
        assert!(error.to_string().contains("receiver rejected pairing"));
    }

    #[test]
    fn strips_control_characters() {
        assert_eq!(clean("TV\n\x1b", 20), "TV  ");
    }

    #[test]
    fn detects_tailscale_routes() {
        assert!(route_uses_tailscale(
            "192.168.1.139 dev tailscale0 table 52 src 100.105.10.51 uid 1000"
        ));
        assert!(route_uses_tailscale(
            "192.168.1.139 via 100.64.0.1 dev tailscale0 src 100.64.0.2"
        ));
        assert!(!route_uses_tailscale(
            "192.168.1.139 dev wlp0s20f3 src 192.168.1.84 uid 1000"
        ));
    }
}

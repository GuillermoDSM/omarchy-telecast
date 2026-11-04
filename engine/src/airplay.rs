use crate::{Events, audio, display::VirtualDisplay};
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

#[derive(Debug, PartialEq, Eq)]
enum Backend {
    DoubleTake,
    Native,
    Invalid,
}

fn backend(value: Option<&str>) -> Backend {
    match value {
        Some("doubletake") => Backend::DoubleTake,
        None | Some("native") | Some("rust") => Backend::Native,
        _ => Backend::Invalid,
    }
}

fn selected_backend() -> Backend {
    backend(std::env::var("OMARCHY_AIRPLAY_BACKEND").ok().as_deref())
}

fn ensure_doubletake() -> Result<()> {
    match selected_backend() {
        Backend::DoubleTake => Ok(()),
        Backend::Native => bail!("DoubleTake no está seleccionado; usa OMARCHY_AIRPLAY_BACKEND=doubletake"),
        Backend::Invalid => bail!("OMARCHY_AIRPLAY_BACKEND debe ser doubletake o native"),
    }
}

fn experimental_audio(value: Option<&str>) -> bool {
    value == Some("1")
}

fn ensure_native_features(features: u64) -> Result<()> {
    if features & (1 << 14) != 0 {
        bail!(
            "Este receptor requiere FairPlay FPSAP, aún no disponible en Rust. Para usar DoubleTake reinicia con OMARCHY_AIRPLAY_BACKEND=doubletake"
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayMode {
    Extended,
    Mirror,
}

fn display_mode(value: Option<&str>) -> Result<DisplayMode> {
    match value {
        None | Some("extended") => Ok(DisplayMode::Extended),
        Some("mirror") => Ok(DisplayMode::Mirror),
        _ => bail!("OMARCHY_AIRPLAY_DISPLAY debe ser extended o mirror"),
    }
}

fn selected_display_mode() -> Result<DisplayMode> {
    display_mode(std::env::var("OMARCHY_AIRPLAY_DISPLAY").ok().as_deref())
}

struct Running {
    child: Child,
    test: bool,
    audio_source: Option<String>,
    display_mode: DisplayMode,
}

/// Owns the DoubleTake daemon used for AirPlay mirroring. The daemon is kept
/// alive between scans so the mDNS cache and pairing state survive popup
/// reopens; it is stopped when the engine exits.
pub struct Manager {
    socket: PathBuf,
    creds: PathBuf,
    log: PathBuf,
    running: Mutex<Option<Running>>,
    native_pin: Mutex<Option<tokio::sync::mpsc::Sender<String>>>,
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
            // A daemon left behind by an earlier worker must never be mistaken
            // for this session's output-guarded process.
            socket: runtime.join(format!("airplay-{}.sock", std::process::id())),
            creds: state.join("airplay-credentials.json"),
            log: state.join("airplay.log"),
            running: Mutex::new(None),
            native_pin: Mutex::new(None),
        })
    }

    pub async fn warm(&self) {
        if selected_backend() != Backend::DoubleTake {
            return;
        }
        if let Ok(mode) = selected_display_mode() {
            let _ = self.ensure(false, None, mode).await;
        }
    }

    pub async fn is_streaming(&self) -> bool {
        if !self.socket.exists() {
            return false;
        }
        self.status()
            .await
            .is_ok_and(|status| status["state"] == "streaming")
    }

    pub async fn devices(&self) -> Result<Vec<Value>> {
        if selected_backend() == Backend::Native {
            let devices =
                crate::airplay_native::manager::Manager::discover(Duration::from_millis(1200))
                    .await?;
            return Ok(devices.into_iter().filter_map(|device| {
                let address = device.addresses.into_iter().next()?;
                let ip = address.ip().to_string();
                let id = device.device_id.unwrap_or_else(|| ip.clone());
                Some(json!({"protocol":"airplay","mac":clean(&id,64),"name":clean(&device.name,120),"model":clean(device.model.as_deref().unwrap_or(""),120),"strength":0,"wfd":false,"ip":ip,"port":address.port()}))
            }).collect());
        }
        ensure_doubletake()?;
        if self.status().await.is_err() {
            self.ensure(false, None, selected_display_mode()?).await?;
        }
        let response = self
            .request(json!({"cmd":"devices"}), REQUEST_TIMEOUT)
            .await?;
        Ok(map_devices(&response))
    }

    pub async fn scan(&self) -> Result<Vec<Value>> {
        let deadline = Instant::now() + SCAN_WAIT;
        let mut devices;
        loop {
            devices = self.devices().await?;
            if !devices.is_empty() || Instant::now() >= deadline {
                break;
            }
            sleep(Duration::from_millis(250)).await;
        }
        Ok(devices)
    }

    pub async fn pin(&self, pin: &str) -> Result<()> {
        if selected_backend() == Backend::Native {
            let sender = self
                .native_pin
                .lock()
                .await
                .clone()
                .context("No hay una solicitud de PIN activa")?;
            return sender
                .try_send(pin.to_owned())
                .context("No se pudo entregar el PIN");
        }
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

    async fn ensure(
        &self,
        test: bool,
        audio_source: Option<&str>,
        display_mode: DisplayMode,
    ) -> Result<()> {
        ensure_doubletake()?;
        let mut guard = self.running.lock().await;
        if let Some(running) = guard.as_mut() {
            if running.test == test
                && running.audio_source.as_deref() == audio_source
                && running.display_mode == display_mode
                && running.child.try_wait()?.is_none()
            {
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
            .arg("-omarchy-output-guard")
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
        command.env("OMARCHY_AIRPLAY_PORTAL_FRESH", "1");
        match display_mode {
            DisplayMode::Extended => {
                command.env("OMARCHY_AIRPLAY_OUTPUT", VirtualDisplay::session_name());
            }
            DisplayMode::Mirror => {
                command.env_remove("OMARCHY_AIRPLAY_OUTPUT");
            }
        }
        if std::env::var_os("OMARCHY_MIRACAST_AIRPLAY_DEBUG").is_some() {
            command.arg("-debug");
        }
        if test {
            command.arg("-test");
        }
        if let Some(source) = audio_source {
            command.arg("-audio-source").arg(source);
        }
        let child = command
            .spawn()
            .with_context(|| format!("No se pudo iniciar {}", binary.display()))?;
        *guard = Some(Running {
            child,
            test,
            audio_source: audio_source.map(str::to_owned),
            display_mode,
        });

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
    receiver_id: String,
    display_mode: Option<DisplayMode>,
    events: Events,
    cancel: CancellationToken,
) {
    if selected_backend() == Backend::Native {
        native_run(
            manager,
            target,
            port,
            test,
            name,
            receiver_id,
            display_mode,
            events,
            cancel,
        )
        .await;
        return;
    }
    let name = clean(&name, 120);
    let mut last_state = String::new();
    let mut audio_warned = false;
    let mut display = None;
    let mut tv_audio = None;
    let result = async {
        check_route(&target).await?;
        ensure_doubletake()?;
        let mode = match display_mode { Some(mode) => mode, None => selected_display_mode()? };
        if !test {
            if mode == DisplayMode::Extended {
                events.state("display", "Creando pantalla extendida para AirPlay");
                display = Some(VirtualDisplay::create().await?);
            }
            if let Some(display) = display.as_ref() {
                events.state("capture", &format!("Selecciona {} en el diálogo de compartir pantalla", display.name));
            } else {
                events.state("capture", "Selecciona la pantalla que deseas duplicar en el portal");
            }
        }
        if !test && experimental_audio(std::env::var("OMARCHY_AIRPLAY_EXPERIMENTAL_TV_SINK").ok().as_deref()) {
            anyhow::ensure!(gstreamer::ElementFactory::find("pulsesrc").is_some(), "Falta pulsesrc; no se capturará una fuente de audio alternativa");
            tv_audio = Some(audio::TvOutput::create().await.context("No se pudo crear la salida experimental para la TV")?);
            tv_audio.as_ref().unwrap().wait_pulse_monitor().await?;
            events.warning("Selecciona Omarchy-AirPlay-TV para enviar audio a la TV; esta ruta es experimental.");
        }
        let source = tv_audio.as_ref().map(|output| output.monitor());
        manager.ensure(test, source.as_deref(), mode).await?;
        events.state(
            "connecting",
            &if test {
                format!("Conectando con {name} por AirPlay; acepta la solicitud en la TV")
            } else if display.is_some() {
                format!("Conectando con {name}; selecciona {} en el portal", VirtualDisplay::session_name())
            } else {
                format!("Conectando con {name}; selecciona la pantalla que deseas duplicar")
            },
        );
        manager.connect(&target, port).await?;
        loop {
            if cancel.is_cancelled() {
                break;
            }
            if let Some(output) = tv_audio.as_ref() { output.check_pulse_monitor().await?; }
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
                        let detail = if has_audio && tv_audio.is_some() {
                            format!("AirPlay activo · selecciona Omarchy-AirPlay-TV para enviar audio a {name}")
                        } else if has_audio {
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
    // Stop capture before unloading its monitor source.
    if tv_audio.is_some() {
        manager.shutdown().await;
    }
    if let Some(mut output) = tv_audio {
        output.remove().await;
    }
    if let Some(display) = display
        && let Err(error) = display.remove().await
    {
        events.state("error", &format!("No se pudo retirar la salida virtual; no se reconectará: {error:#}"));
        return;
    }
    match result {
        Ok(()) => events.state("idle", "Sesión AirPlay terminada"),
        Err(error) => events.state("error", &format!("{error:#}")),
    }
}

#[derive(Default)]
struct NativeResources {
    display: Option<VirtualDisplay>,
    portal: Option<crate::media::Capture>,
    output: Option<audio::TvOutput>,
    previous_sink: Option<String>,
}

fn random_uuid() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h: String = bytes.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

// The locally generated wrapping key is kept in a separate 0600 file in a
// 0700 directory. It protects accidental disclosure, not a compromised user.
fn native_key() -> Result<zeroize::Zeroizing<[u8; 32]>> {
    use rand_core::{OsRng, RngCore};
    use std::{
        io::{Read, Write},
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)?;
    set_dir_mode(&dir)?;
    let path = dir.join("airplay-native-key");
    let mut fresh = zeroize::Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(&mut *fresh);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(&*fresh)?;
            file.sync_all()?;
            return Ok(fresh);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    let m = file.metadata()?;
    anyhow::ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.len() == 32,
        "Clave AirPlay nativa con permisos o tamaño inseguros"
    );
    file.read_exact(&mut *fresh)?;
    Ok(fresh)
}

fn native_credential_path(receiver_id: &str) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    anyhow::ensure!(
        !receiver_id.is_empty() && receiver_id.len() <= 128,
        "Identidad AirPlay inválida"
    );
    let hash = Sha256::digest(receiver_id.to_ascii_uppercase().as_bytes());
    let name: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    Ok(state_dir()?.join(format!("airplay-native-{name}.cred")))
}

async fn native_run(
    manager: std::sync::Arc<Manager>,
    target: String,
    port: u16,
    test: bool,
    name: String,
    receiver_id: String,
    display_mode: Option<DisplayMode>,
    events: Events,
    cancel: CancellationToken,
) {
    use crate::airplay_native::{
        capture::Capture as NativeCapture, manager::Manager as NativeManager,
        pairing::Credentials,
        sources::{Kind as SourceKind, VideoSources},
        worker,
    };
    let mut resources = NativeResources::default();
    let name = clean(&name, 120);
    let result = {
        let operation = async {
            check_route(&target).await?;
            // Native mode comes from the portal selection; still validate any
            // inherited environment override for the fallback backend.
            let _mode = match display_mode {
                Some(mode) => mode,
                None => selected_display_mode()?,
            };
            let key = native_key()?;
            let credential_path = native_credential_path(&receiver_id)?;
            let mut credentials = match Credentials::load(&credential_path, &key) {
                Ok(credentials) => Some(credentials),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    None
                }
                Err(error) => {
                    return Err(error.context("No se pueden leer las credenciales AirPlay nativas"));
                }
            };
            let identifier = credentials
                .as_ref()
                .map(|c| c.identifier.clone())
                .unwrap_or_else(random_uuid);
            let address = std::net::SocketAddr::new(target.parse()?, port);
            events.state(
                "connecting",
                &format!("Conectando con {name} mediante AirPlay Rust"),
            );
            let mut native =
                NativeManager::connect(address, identifier.clone(), "Omarchy".into()).await?;
            events.trace("native", "receiver_info_ok");
            ensure_native_features(native.features)?;
            if let Some(stored) = credentials.as_ref() {
                native.resume(stored).await.context("Falló la verificación del receptor guardado; no se reemplazarán sus credenciales automáticamente")?;
            } else {
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                *manager.native_pin.lock().await = Some(tx);
                native.request_pin().await?;
                events.send(json!({"event":"pin_required","kind":"pin","device":name}));
                events.state("pairing", &format!("Introduce el PIN que muestra {name}"));
                let pin = tokio::select! {
                    _ = cancel.cancelled() => bail!("Emparejamiento cancelado"),
                    pin = timeout(Duration::from_secs(120), rx.recv()) => pin.context("Tiempo de PIN agotado")?.context("Solicitud de PIN cerrada")?,
                };
                *manager.native_pin.lock().await = None;
                let paired = native
                    .submit_pin(&pin)
                    .await
                    .context("Falló el emparejamiento AirPlay")?;
                paired
                    .save(&credential_path, &key)
                    .context("No se pudieron guardar las credenciales AirPlay")?;
                credentials = Some(paired);
            }
            events.trace("native", "pair_verify_ok");
            let mut virtual_name = None;
            let mut initial_kind = SourceKind::Virtual;
            let video_source = if test {
                crate::media::Source::Test
            } else {
                // The portal is the only mode picker: choosing the temporary
                // output extends the desktop, choosing an existing monitor
                // mirrors it. The unused temporary output is then released.
                events.state("display", "Creando pantalla virtual para AirPlay");
                resources.display = Some(VirtualDisplay::create().await?);
                virtual_name = resources.display.as_ref().map(|d| d.name.clone());
                resources.portal = Some(crate::media::Capture::new().await?);
                {
                    let portal = resources.portal.as_mut().unwrap();
                    events.state(
                        "capture",
                        "Elige en el portal el monitor virtual para extender o una pantalla existente para duplicar",
                    );
                    timeout(Duration::from_secs(120), portal.select_mirror())
                        .await
                        .context("Tiempo de selección agotado")??;
                    if let Some(name) = virtual_name.as_deref()
                        && portal
                            .mapping()
                            .is_some_and(|selected| selected != name)
                    {
                        events.trace("native", "mirror_existing_output");
                        initial_kind = SourceKind::Laptop;
                        if let Some(display) = resources.display.take() {
                            display.remove().await?;
                        }
                    }
                }
                resources.portal.as_ref().unwrap().source()?
            };
            events.trace("native", "portal_source_ok");
            resources.output = Some(audio::TvOutput::create().await?);
            events.trace("native", "audio_output_created");
            let output = resources.output.as_ref().unwrap();
            output.wait_pulse_monitor().await?;
            resources.previous_sink = audio::route_to_tv(output.node_name()).await;
            events.trace("native", "audio_monitor_ready");
            if resources.previous_sink.is_some() {
                events.trace("native", "audio_routed_to_tv");
            }
            let video = NativeCapture::video(
                video_source,
                1920,
                1080,
                30,
                crate::airplay_native::capture::DEFAULT_BITRATE_KBPS,
            )?;
            let audio = NativeCapture::audio(&output.monitor())?;
            events.trace("native", "capture_pipelines_started");
            timeout(Duration::from_secs(10), async {
                loop {
                    if video.pull()?.is_some() {
                        break Ok::<(), anyhow::Error>(());
                    }
                    // Surface pipeline failures even when the TV sink is silent.
                    let _ = audio.pull()?;
                    sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .context("La captura de vídeo AirPlay no produjo imágenes")??;
            events.trace("native", "capture_first_frame");
            let sources = if test {
                VideoSources::test(video)
            } else {
                let portal = resources
                    .portal
                    .take()
                    .context("El portal de captura no está disponible")?;
                let name = if initial_kind == SourceKind::Virtual {
                    virtual_name.take()
                } else {
                    None
                };
                VideoSources::new(name, portal, video, initial_kind)
            };
            let (event_tx, event_rx) = tokio::sync::mpsc::channel(32);
            let device_id = format!(
                "02:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                key[0], key[1], key[2], key[3], key[4]
            );
            let (session, event_rx) = match native
                .start(random_uuid(), device_id.clone(), event_tx)
                .await
            {
                Ok(session) => (session, event_rx),
                Err(error)
                    if error
                        .downcast_ref::<crate::airplay_native::session::MissingReceiverTimeline>()
                        .is_some() =>
                {
                    // DoubleTake's verified behaviour: some receivers (LG webOS 5,
                    // Samsung) advertise PTP but name no timeline. Retry once on a
                    // fresh, re-verified connection over NTP.
                    events.trace("native", "ptp_timeline_missing_retry_ntp");
                    let stored = credentials
                        .as_ref()
                        .context("No hay credenciales para reintentar AirPlay por NTP")?;
                    // Give the receiver a moment to release the rejected session
                    // before a fresh control connection is verified.
                    sleep(Duration::from_millis(400)).await;
                    let mut retry =
                        NativeManager::connect(address, identifier, "Omarchy".into()).await?;
                    events.trace("native", "ntp_reconnect_ok");
                    ensure_native_features(retry.features)?;
                    retry
                        .resume(stored)
                        .await
                        .context("Falló la verificación al reintentar AirPlay por NTP")?;
                    events.trace("native", "ntp_reverify_ok");
                    let (tx, rx) = tokio::sync::mpsc::channel(32);
                    let session = retry
                        .start_with_timing(
                            random_uuid(),
                            device_id,
                            tx,
                            Some(crate::airplay_native::session::TimingMode::Ntp),
                        )
                        .await?;
                    (session, rx)
                }
                Err(error) => return Err(error),
            };
            events.trace("native", "setup_record_complete");
            events.send(json!({"event":"airplay","device":name,"audio":true}));
            events.state(
                "streaming",
                &format!(
                    "AirPlay Rust activo · selecciona Omarchy-AirPlay-TV para audio en {name}"
                ),
            );
            worker::run(session, sources, audio, output, event_rx, cancel.clone(), events.clone()).await
        };
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => result,
            _ = cancel.cancelled() => match timeout(Duration::from_secs(4), &mut operation).await { Ok(result) => result, Err(_) => Ok(()) },
        }
    };
    *manager.native_pin.lock().await = None;
    if let Some(output) = resources.output.as_ref() {
        audio::restore_from_tv(output.node_name(), resources.previous_sink.as_deref()).await;
    }
    if let Some(mut output) = resources.output.take() {
        output.remove().await;
    }
    // Preserve the real media error even when a later cleanup step also fails;
    // previously the cleanup message replaced the cause.
    let operation_error = match &result {
        Err(error) if !cancel.is_cancelled() => {
            let detail: String = format!("{error:#}").chars().take(240).collect();
            events.send(json!({"event":"trace","stage":"native_error","detail":"operation_failed","error":detail}));
            Some(format!("{error:#}"))
        }
        _ => None,
    };
    let mut cleanup_error = None;
    if let Some(portal) = resources.portal.take() {
        match timeout(Duration::from_secs(3), portal.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => cleanup_error = Some(format!("No se pudo cerrar el portal: {error:#}")),
            Err(_) => cleanup_error = Some("El cierre del portal excedió el tiempo límite".to_owned()),
        }
    }
    if let Some(display) = resources.display.take()
        && let Err(error) = display.remove().await
    {
        cleanup_error = Some(format!("No se pudo retirar la salida virtual: {error:#}"));
    }
    if let Some(error) = cleanup_error {
        let cause = operation_error
            .map(|cause| format!("; fallo previo: {cause}"))
            .unwrap_or_default();
        events.state(
            "error",
            &format!("{error}{cause}; no se reconectará automáticamente"),
        );
        return;
    }
    match result {
        Ok(()) => events.state("idle", "Sesión AirPlay terminada"),
        Err(_) if cancel.is_cancelled() => events.state("idle", "Sesión AirPlay cancelada"),
        Err(error) => events.state("error", &format!("{error:#}")),
    }
}

pub async fn doctor() -> Vec<Value> {
    let mut checks = Vec::new();
    if selected_backend() == Backend::Native {
        for (name, ok, detail) in [
            (
                "Vídeo AirPlay Rust",
                ["x264enc", "h264parse", "pipewiresrc"]
                    .iter()
                    .all(|s| gstreamer::ElementFactory::find(s).is_some()),
                "x264enc, h264parse y pipewiresrc",
            ),
            (
                "Audio AirPlay Rust",
                audio::native_available().await
                    && ["pulsesrc", "audioconvert", "audioresample"]
                        .iter()
                        .all(|s| gstreamer::ElementFactory::find(s).is_some()),
                "pw-cli, pw-dump, pipewire-pulse y GStreamer PulseAudio",
            ),
        ] {
            checks.push(json!({"name":name,"ok":ok,"detail":detail,"group":"airplay"}));
        }
        return checks;
    }
    if let Err(error) = ensure_doubletake() {
        checks.push(json!({"name":"Backend AirPlay", "ok":false, "detail":format!("{error:#}"), "group":"airplay"}));
        return checks;
    }
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
        "detail": if pactl { "pactl disponible; se captura la salida predeterminada por defecto" } else { "Falta pactl (libpulse)" },
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
    fn backend_and_modes_are_explicit_and_fail_closed() {
        assert_eq!(backend(None), Backend::Native);
        assert_eq!(backend(Some("doubletake")), Backend::DoubleTake);
        assert_eq!(backend(Some("native")), Backend::Native);
        assert_eq!(backend(Some("rust")), Backend::Native);
        assert_eq!(backend(Some("unknown")), Backend::Invalid);
        assert!(!experimental_audio(None));
        assert!(experimental_audio(Some("1")));
        assert!(ensure_native_features(0x038bcb46007f8ad0).is_ok());
        assert!(
            ensure_native_features(1 << 14)
                .unwrap_err()
                .to_string()
                .contains("FPSAP")
        );
        assert_eq!(display_mode(None).unwrap(), DisplayMode::Extended);
        assert_eq!(display_mode(Some("mirror")).unwrap(), DisplayMode::Mirror);
        assert!(display_mode(Some("unknown")).is_err());
    }

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

use crate::{Events, audio, display::VirtualDisplay};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::Command,
    sync::Mutex,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

fn ensure_native_features(features: u64) -> Result<()> {
    if features & (1 << 14) != 0 {
        bail!(
            "El receptor anuncia FairPlay SAP (FPSAP), que el motor AirPlay nativo todavía no implementa"
        );
    }
    Ok(())
}

/// Owns only the PIN handoff between the media task and the IPC command loop.
pub struct Manager {
    native_pin: Mutex<Option<tokio::sync::mpsc::Sender<String>>>,
}

impl Manager {
    pub fn new() -> Result<Self> {
        Ok(Self {
            native_pin: Mutex::new(None),
        })
    }

    pub async fn devices(&self) -> Result<Vec<Value>> {
        let devices =
            crate::airplay_native::manager::Manager::discover(Duration::from_millis(1200)).await?;
        Ok(devices
            .into_iter()
            .filter_map(|device| {
                let address = device.addresses.into_iter().next()?;
                let ip = address.ip().to_string();
                let id = device.device_id.unwrap_or_else(|| ip.clone());
                Some(json!({
                    "protocol":"airplay",
                    "mac":clean(&id,64),
                    "name":clean(&device.name,120),
                    "model":clean(device.model.as_deref().unwrap_or(""),120),
                    "strength":0,
                    "ip":ip,
                    "port":address.port()
                }))
            })
            .collect())
    }

    pub async fn pin(&self, pin: &str) -> Result<()> {
        let sender = self
            .native_pin
            .lock()
            .await
            .clone()
            .context("No hay una solicitud de PIN activa")?;
        sender
            .try_send(pin.to_owned())
            .context("No se pudo entregar el PIN")
    }
}

pub async fn run(
    manager: std::sync::Arc<Manager>,
    target: String,
    port: u16,
    test: bool,
    name: String,
    receiver_id: String,
    events: Events,
    cancel: CancellationToken,
) {
    native_run(
        manager,
        target,
        port,
        test,
        name,
        receiver_id,
        events,
        cancel,
    )
    .await;
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
    events: Events,
    cancel: CancellationToken,
) {
    use crate::airplay_native::{
        capture::Capture as NativeCapture,
        manager::Manager as NativeManager,
        pairing::Credentials,
        sources::{Kind as SourceKind, VideoSources},
        worker,
    };
    let mut resources = NativeResources::default();
    let name = clean(&name, 120);
    let result = {
        let operation = async {
            check_route(&target).await?;
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
                        && portal.mapping().is_some_and(|selected| selected != name)
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
                    // Verified receiver behaviour: some receivers (LG webOS 5,
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
            worker::run(
                session,
                sources,
                audio,
                output,
                event_rx,
                cancel.clone(),
                events.clone(),
            )
            .await
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
            Ok(Err(error)) => {
                cleanup_error = Some(format!("No se pudo cerrar el portal: {error:#}"))
            }
            Err(_) => {
                cleanup_error = Some("El cierre del portal excedió el tiempo límite".to_owned())
            }
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
    fn native_features_fail_closed_on_fpsap() {
        assert!(ensure_native_features(0x038bcb46007f8ad0).is_ok());
        assert!(
            ensure_native_features(1 << 14)
                .unwrap_err()
                .to_string()
                .contains("FPSAP")
        );
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

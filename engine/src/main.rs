mod airplay;
mod airplay_native;
mod audio;
mod display;
mod media;
mod session_log;
mod stdio;

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

const DISCOVERY_WINDOW: Duration = Duration::from_secs(12);
const DISCOVERY_POLL: Duration = Duration::from_millis(500);
const DISCOVERY_SOURCE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct Events(
    mpsc::Sender<Value>,
    Option<Arc<Mutex<session_log::SessionLog>>>,
);

impl Events {
    fn send(&self, value: Value) {
        if let Some(log) = &self.1 {
            let safe = match value["event"].as_str() {
                Some("state") => Some(json!({"event":"state","phase":value["phase"]})),
                Some("trace") => Some(value.clone()),
                _ => None,
            };
            if let Some(event) = safe
                && let Ok(mut log) = log.lock()
            {
                let _ = log.write(event);
            }
        }
        let _ = self.0.try_send(value);
    }
    fn trace(&self, stage: &str, detail: &str) {
        self.send(json!({"event":"trace","stage":stage,"detail":detail}));
    }
    fn with_log(&self, log: session_log::SessionLog) -> Self {
        Self(self.0.clone(), Some(Arc::new(Mutex::new(log))))
    }
    fn state(&self, phase: &str, detail: &str) {
        let discovering = phase == "searching";
        let busy = !matches!(phase, "idle" | "error" | "searching");
        self.send(json!({
            "event":"state",
            "phase":phase,
            "detail":detail,
            "busy":busy,
            "discovering":discovering
        }));
    }
    fn warning(&self, detail: &str) {
        self.send(json!({"event":"warning","detail":detail}));
    }
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Doctor,
    Scan,
    Connect {
        #[serde(default)]
        mac: String,
        #[serde(default)]
        ip: String,
        #[serde(default)]
        port: u16,
        #[serde(default)]
        name: String,
        #[serde(default)]
        test: bool,
    },
    Pin {
        pin: String,
    },
    Stop,
    Quit,
}

enum DiscoveryEvent {
    Peers { generation: u64, peers: Vec<Value> },
    Warning { generation: u64, detail: String },
    Finished { generation: u64, peers: Vec<Value> },
}

struct DiscoveryJob {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

async fn doctor() -> Value {
    let mut checks = Vec::new();
    for factory in [
        "pipewiresrc",
        "videoconvert",
        "videoscale",
        "videorate",
        "h264parse",
        "appsink",
    ] {
        let ok = gstreamer::ElementFactory::find(factory).is_some();
        checks.push(json!({"name":factory,"ok":ok,"detail":if ok { "Disponible" } else { "Falta el componente de GStreamer" }}));
    }
    let x264 = gstreamer::ElementFactory::find("x264enc").is_some();
    checks.push(json!({
        "name":"Codificador H.264",
        "ok":x264,
        "detail":if x264 { "x264 (software, estable)" } else { "Falta x264enc" }
    }));
    let audio = audio::native_available().await
        && ["pulsesrc", "audioconvert", "audioresample"]
            .iter()
            .all(|factory| gstreamer::ElementFactory::find(factory).is_some());
    checks.push(json!({
        "name":"Audio",
        "ok":audio,
        "detail":if audio { "PipeWire/PulseAudio y GStreamer disponibles" } else { "Falta pw-cli, pw-dump, pipewire-pulse o componentes de GStreamer" }
    }));
    let hypr = display::monitors().await;
    checks.push(json!({"name":"Hyprland","ok":hypr.is_ok(),"detail":hypr.err().map_or("Disponible".to_string(),|error| error.to_string())}));
    checks.push(json!({
        "name":"Portal de pantalla",
        "ok":gstreamer::ElementFactory::find("pipewiresrc").is_some(),
        "detail":"La selección de pantalla se solicita al conectar"
    }));
    let ready = checks.iter().all(|check| check["ok"] == true);
    json!({
        "event":"diagnostics",
        "checks":checks,
        "ready":ready,
        "airplay_ready":ready,
        "experimental":true,
        "firewall_changed":false
    })
}

// Limit even malformed/no-newline input; never accumulate an unbounded line.
async fn read_line(
    reader: &mut (impl AsyncBufReadExt + Unpin),
    bytes: &mut Vec<u8>,
) -> Result<Option<String>> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                bail!("Comando JSON incompleto")
            };
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if bytes.len() + count > 16384 {
            bail!("Comando JSON demasiado largo");
        }
        let ended = available[count - 1] == b'\n';
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if ended {
            return Ok(Some(String::from_utf8(std::mem::take(bytes))?));
        }
    }
}

fn peer_signature(peers: &[Value]) -> String {
    peers
        .iter()
        .map(|peer| {
            format!(
                "{}|{}|{}|{}|{}",
                peer["mac"].as_str().unwrap_or_default(),
                peer["ip"].as_str().unwrap_or_default(),
                peer["port"].as_u64().unwrap_or(0),
                peer["name"].as_str().unwrap_or_default(),
                peer["model"].as_str().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn publish_peers(
    tx: &mpsc::Sender<DiscoveryEvent>,
    generation: u64,
    peers: &[Value],
    last_signature: &mut String,
) {
    let signature = peer_signature(peers);
    if signature == *last_signature {
        return;
    }
    if tx
        .try_send(DiscoveryEvent::Peers {
            generation,
            peers: peers.to_vec(),
        })
        .is_ok()
    {
        *last_signature = signature;
    }
}

fn publish_warning(
    tx: &mpsc::Sender<DiscoveryEvent>,
    generation: u64,
    warnings: &mut HashSet<String>,
    detail: String,
) {
    if !warnings.insert(detail.clone()) {
        return;
    }
    let _ = tx.try_send(DiscoveryEvent::Warning { generation, detail });
}

async fn discover(
    airplay: Arc<airplay::Manager>,
    tx: mpsc::Sender<DiscoveryEvent>,
    generation: u64,
    cancel: CancellationToken,
) {
    let deadline = Instant::now() + DISCOVERY_WINDOW;
    let mut warnings = HashSet::new();
    let mut last_signature = String::new();
    let mut peers = Vec::new();

    loop {
        if cancel.is_cancelled() {
            return;
        }
        match timeout(DISCOVERY_SOURCE_TIMEOUT, airplay.devices()).await {
            Ok(Ok(found)) => {
                peers = found;
                publish_peers(&tx, generation, &peers, &mut last_signature);
            }
            Ok(Err(error)) => publish_warning(
                &tx,
                generation,
                &mut warnings,
                format!("AirPlay: {error:#}"),
            ),
            Err(_) => publish_warning(
                &tx,
                generation,
                &mut warnings,
                "AirPlay: la consulta superó 3 segundos".to_string(),
            ),
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = sleep(DISCOVERY_POLL) => {}
        }
    }

    let _ = tx
        .send(DiscoveryEvent::Finished { generation, peers })
        .await;
}

async fn collect_peers(manager: &airplay::Manager) -> (Vec<Value>, Vec<String>) {
    match timeout(Duration::from_secs(12), manager.devices()).await {
        Ok(Ok(peers)) => (peers, Vec::new()),
        Ok(Err(error)) => (Vec::new(), vec![format!("AirPlay: {error:#}")]),
        Err(_) => (
            Vec::new(),
            vec!["AirPlay: la búsqueda superó 12 segundos".to_string()],
        ),
    }
}

async fn serve() -> Result<()> {
    let (tx, mut rx) = mpsc::channel(128);
    let events = Events(tx, None);
    let (scan_tx, mut scan_rx) = mpsc::channel(32);
    let airplay = Arc::new(airplay::Manager::new()?);
    display::cleanup_stale().await;
    let mut stdin = BufReader::new(stdio::Input::new()?);
    let mut command_buffer = Vec::new();
    let mut stdout = stdio::Output::new()?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut job: Option<(CancellationToken, JoinHandle<()>)> = None;
    let mut job_terminal = false;
    let mut discovery: Option<DiscoveryJob> = None;
    let mut scan_generation: u64 = 0;
    events.send(json!({"event":"ready","version":env!("CARGO_PKG_VERSION")}));
    events.state("idle", "AirPlay experimental");
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            Some(event) = rx.recv() => {
                if job.is_some()
                    && event["event"] == "state"
                    && matches!(event["phase"].as_str(), Some("idle" | "error"))
                {
                    job_terminal = true;
                }
                let mut bytes = serde_json::to_vec(&event)?;
                bytes.push(b'\n');
                let written = timeout(Duration::from_secs(2),async {
                    stdout.write_all(&bytes).await?;
                    stdout.flush().await
                }).await;
                if !matches!(written,Ok(Ok(()))) { break; }
            }
            Some(scan_event) = scan_rx.recv() => {
                match scan_event {
                    DiscoveryEvent::Peers { generation, peers }
                        if generation == scan_generation && discovery.is_some() =>
                    {
                        events.send(json!({"event":"peers","peers":peers}));
                    }
                    DiscoveryEvent::Warning { generation, detail }
                        if generation == scan_generation && discovery.is_some() =>
                    {
                        events.warning(&detail);
                    }
                    DiscoveryEvent::Finished { generation, peers }
                        if generation == scan_generation && discovery.is_some() =>
                    {
                        events.send(json!({"event":"peers","peers":peers}));
                        events.state("idle",if peers.is_empty() {
                            "No se encontraron receptores AirPlay. Comprueba que la TV esté en AirPlay"
                        } else {
                            "Búsqueda terminada. Elige una TV para conectar"
                        });
                        if let Some(done) = discovery.take() {
                            let _ = done.task.await;
                        }
                        scan_generation = scan_generation.wrapping_add(1);
                    }
                    _ => {}
                }
            }
            line = read_line(&mut stdin, &mut command_buffer) => {
                let line = match line { Ok(Some(line)) => line, Ok(None) => break, Err(error) => { events.warning(&error.to_string()); break; } };
                let request = match serde_json::from_str::<Request>(&line) { Ok(request) => request,Err(error) => { events.warning(&format!("Comando inválido: {error}")); continue; } };
                if job.as_ref().is_some_and(|(_,task)| task.is_finished() || job_terminal)
                    && let Some((_,task)) = job.take() {
                    job_terminal = false;
                    if let Err(error) = task.await {
                        events.state("error",&format!("El motor terminó inesperadamente: {error}"));
                    }
                }
                match request {
                    Request::Quit => break,
                    Request::Stop => {
                        if let Some((cancel,_)) = &job {
                            events.state("stopping","Cancelando y liberando los recursos de esta sesión");
                            cancel.cancel();
                        } else if let Some(mut active) = discovery.take() {
                            active.cancel.cancel();
                            if timeout(Duration::from_secs(1), &mut active.task).await.is_err() {
                                active.task.abort();
                            }
                            scan_generation = scan_generation.wrapping_add(1);
                            events.state("idle","Búsqueda cancelada");
                        }
                    }
                    Request::Doctor if job.is_none() && discovery.is_none() => {
                        events.send(timeout(Duration::from_secs(12),doctor()).await.context("El diagnóstico excedió el tiempo límite")?);
                    }
                    Request::Scan if job.is_none() => {
                        if discovery.as_ref().is_some_and(|active| active.task.is_finished()) {
                            discovery = None;
                            scan_generation = scan_generation.wrapping_add(1);
                        }
                        if discovery.is_some() {
                            events.warning("La búsqueda ya está en curso");
                        } else {
                            scan_generation = scan_generation.wrapping_add(1);
                            let generation = scan_generation;
                            let cancel = CancellationToken::new();
                            let task = tokio::spawn(discover(
                                airplay.clone(),
                                scan_tx.clone(),
                                generation,
                                cancel.clone(),
                            ));
                            discovery = Some(DiscoveryJob { cancel, task });
                            events.send(json!({"event":"peers","peers":[]}));
                            events.state("searching","Buscando receptores AirPlay");
                        }
                    }
                    Request::Pin { pin } if job.is_some() => {
                        if pin.is_empty() || pin.len() > 128 {
                            events.warning("El PIN o contraseña no es válido");
                        } else if let Err(error) = airplay.pin(&pin).await {
                            events.warning(&format!("No se pudo enviar el PIN: {error:#}"));
                        }
                    }
                    Request::Pin { .. } => {
                        events.warning("No hay una solicitud de PIN AirPlay activa");
                    }
                    Request::Connect { mac,ip,port,name,test } if job.is_none() => {
                        if let Some(mut active) = discovery.take() {
                            active.cancel.cancel();
                            if timeout(Duration::from_secs(1), &mut active.task).await.is_err() {
                                active.task.abort();
                            }
                            scan_generation = scan_generation.wrapping_add(1);
                        }
                        if ip.parse::<std::net::IpAddr>().is_err() {
                            events.state("error","La dirección del receptor AirPlay no es válida");
                        } else if port == 0 {
                            events.state("error","El receptor AirPlay no anunció un puerto válido");
                        } else {
                            let cancel = CancellationToken::new();
                            let session_events = match session_log::SessionLog::open() {
                                Ok(log) => {
                                    events.send(json!({"event":"log","path":log.path.to_string_lossy()}));
                                    events.with_log(log)
                                },
                                Err(error) => { events.warning(&format!("No se pudo crear un registro privado: {error:#}. La conexión continuará sin registro")); events.clone() }
                            };
                            session_events.trace("session",if test {"airplay-pattern"} else {"airplay"});
                            let display_name = if name.is_empty() { ip.clone() } else { name };
                            let task = tokio::spawn(airplay::run(
                                airplay.clone(),
                                ip,
                                port,
                                test,
                                display_name,
                                mac,
                                session_events,
                                cancel.clone(),
                            ));
                            job_terminal = false;
                            job = Some((cancel,task));
                        }
                    }
                    _ => events.warning("Ya hay una operación activa. Pulsa Detener antes de iniciar otra"),
                }
            }
        }
    }
    // EOF (widget removal/hot-reload), SIGTERM, Ctrl+C and Quit all take the
    // same cleanup path. Do not abort the session task before it can clean up.
    if let Some(mut active) = discovery.take() {
        active.cancel.cancel();
        if timeout(Duration::from_secs(1), &mut active.task)
            .await
            .is_err()
        {
            active.task.abort();
        }
    }
    if let Some((cancel, task)) = job {
        cancel.cancel();
        let _ = timeout(Duration::from_secs(15), task).await;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    gstreamer::init().context("No se pudo iniciar GStreamer")?;
    match std::env::args().nth(1).as_deref() {
        Some("serve") => serve().await,
        Some("doctor") => {
            println!("{}", doctor().await);
            Ok(())
        }
        Some("scan") => {
            let manager = airplay::Manager::new()?;
            let (peers, warnings) = collect_peers(&manager).await;
            println!("{}", json!({"peers":peers,"warnings":warnings}));
            Ok(())
        }
        _ => {
            eprintln!("Uso: omarchy-telecast {{serve|doctor|scan}}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn input_is_bounded_and_cancellation_safe() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        writer.write_all(b"{\"command\":").await.unwrap();
        assert!(
            timeout(
                Duration::from_millis(10),
                read_line(&mut reader, &mut bytes)
            )
            .await
            .is_err()
        );
        writer.write_all(b"\"stop\"}\n").await.unwrap();
        let line = read_line(&mut reader, &mut bytes).await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<Request>(&line).unwrap(),
            Request::Stop
        ));
        let oversized = vec![b'x'; 20000];
        let mut reader = BufReader::new(oversized.as_slice());
        assert!(read_line(&mut reader, &mut Vec::new()).await.is_err());
    }

    #[test]
    fn command_arguments_are_strict() {
        assert!(matches!(
            serde_json::from_str::<Request>(
                r#"{"command":"connect","ip":"192.168.1.2","port":7000}"#
            ),
            Ok(Request::Connect { .. })
        ));
        assert!(
            serde_json::from_str::<Request>(
                r#"{"command":"connect","mac":"00:11:22:33:44:55","test":true}"#
            )
            .is_ok()
        );
        assert!(
            serde_json::from_str::<Request>(
                r#"{"command":"connect","mac":"00:11:22:33:44:55","shell":"reboot"}"#
            )
            .is_err()
        );
    }
}

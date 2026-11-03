mod abr;
mod airplay;
mod audio;
mod display;
mod media;
mod nm;
mod rtcp;
mod rtsp;
mod send_queue;
mod session;
mod session_log;
mod stdio;
mod wfd;

use std::{
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
    time::timeout,
};
use tokio_util::sync::CancellationToken;

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
                Some("metrics") => Some(
                    json!({"event":"metrics","kbps":value["kbps"],"feedback":value["feedback"],"packets":value["packets"],"captured":value["captured"],"encoded":value["encoded"],"produced":value["produced"],"queue_dropped":value["queue_dropped"],"queue_expired":value["queue_expired"],"queue_overflow":value["queue_overflow"],"queue_len":value["queue_len"],"queue_peak":value["queue_peak"],"queue_oldest_ms":value["queue_oldest_ms"],"packet_max_bytes":value["packet_max_bytes"],"keyframe_requests":value["keyframe_requests"],"blocked_current_ms":value["blocked_current_ms"],"blocked_max_ms":value["blocked_max_ms"],"send_timeouts":value["send_timeouts"],"send_would_block":value["send_would_block"],"send_memory_errors":value["send_memory_errors"],"width":value["width"],"height":value["height"],"fps":value["fps"]}),
                ),
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
        self.send(json!({"event":"state","phase":phase,"detail":detail,"busy":!matches!(phase,"idle"|"error")}));
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
        protocol: Option<String>,
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

async fn doctor() -> Value {
    let mut checks = Vec::new();
    for factory in [
        "pipewiresrc",
        "videoconvert",
        "videoscale",
        "videorate",
        "x264enc",
        "h264parse",
        "mpegtsmux",
        "rtpmp2tpay",
        "appsink",
    ] {
        let ok = gstreamer::ElementFactory::find(factory).is_some();
        checks.push(json!({"name":factory,"ok":ok,"detail":if ok { "Disponible" } else { "Falta el componente de GStreamer" }}));
    }
    let hypr = display::monitors().await;
    checks.push(json!({"name":"Hyprland","ok":hypr.is_ok(),"detail":hypr.err().map_or("Disponible".to_string(),|e| e.to_string())}));
    let network = async {
        let manager = nm::Manager::new().await?;
        manager.peers(false).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    checks.push(json!({"name":"NetworkManager Wi-Fi Direct","ok":network.is_ok(),"detail":network.err().map_or("Disponible; compatibilidad de la TV todavía no verificada".to_string(),|e| e.to_string())}));
    let audio = media::audio_available();
    checks.push(json!({"name":"Audio AAC (opcional)", "ok":audio, "detail":if audio { "Componentes disponibles; falta comprobar pactl, la salida y la TV" } else { "Faltan componentes GStreamer para AAC; se intentará vídeo sin audio" }}));
    checks.extend(airplay::doctor().await);
    let ready = checks
        .iter()
        .filter(|check| check["group"] != "airplay")
        .all(|check| check["ok"] == true);
    let airplay_ready = checks
        .iter()
        .filter(|check| check["group"] == "airplay")
        .all(|check| check["ok"] == true);
    json!({
        "event":"diagnostics",
        "checks":checks,
        "ready":ready,
        "airplay_ready":airplay_ready,
        "experimental":true,
        "audio":audio,
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

async fn collect_peers(manager: &airplay::Manager) -> (Vec<Value>, Vec<String>) {
    let (airplay_result, miracast_result) = tokio::join!(
        manager.scan(),
        timeout(Duration::from_secs(12), async {
            nm::Manager::new().await?.peers(true).await
        })
    );
    let mut peers = Vec::new();
    let mut warnings = Vec::new();
    match airplay_result {
        Ok(mut found) => peers.append(&mut found),
        Err(error) => warnings.push(format!("AirPlay: {error:#}")),
    }
    match miracast_result {
        Ok(Ok(found)) => {
            for peer in found {
                if let Ok(mut value) = serde_json::to_value(peer) {
                    value["protocol"] = json!("miracast");
                    peers.push(value);
                }
            }
        }
        Ok(Err(error)) => warnings.push(format!("Miracast: {error:#}")),
        Err(_) => warnings.push("Miracast: la búsqueda superó 12 segundos".to_string()),
    }
    peers.sort_by(|left, right| {
        let left_airplay = left["protocol"] == "airplay";
        let right_airplay = right["protocol"] == "airplay";
        right_airplay
            .cmp(&left_airplay)
            .then_with(|| left["name"].as_str().cmp(&right["name"].as_str()))
    });
    (peers, warnings)
}

async fn serve() -> Result<()> {
    let (tx, mut rx) = mpsc::channel(128);
    let events = Events(tx, None);
    let airplay = Arc::new(airplay::Manager::new()?);
    if !airplay.is_streaming().await {
        audio::cleanup_stale().await;
    }
    let warm_airplay = airplay.clone();
    tokio::spawn(async move {
        warm_airplay.warm().await;
    });
    let mut stdin = BufReader::new(stdio::Input::new()?);
    let mut command_buffer = Vec::new();
    let mut stdout = stdio::Output::new()?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut job: Option<(CancellationToken, JoinHandle<()>)> = None;
    let mut job_terminal = false;
    events.send(json!({"event":"ready","version":env!("CARGO_PKG_VERSION")}));
    events.state(
        "idle",
        "Miracast y AirPlay experimentales · audio en la TV cuando la sesión lo permite",
    );
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
                        if let Some((cancel,_)) = &job { events.state("stopping","Cancelando y liberando los recursos de esta sesión"); cancel.cancel(); }
                    }
                    Request::Doctor if job.is_none() => {
                        events.send(timeout(Duration::from_secs(12),doctor()).await.context("El diagnóstico excedió el tiempo límite")?);
                    }
                    Request::Scan if job.is_none() => {
                        let cancel = CancellationToken::new();
                        let token = cancel.clone();
                        let events = events.clone();
                        let airplay = airplay.clone();
                        let task = tokio::spawn(async move {
                            events.state("searching","Buscando receptores AirPlay y Miracast");
                            let (peers, warnings) = tokio::select! {
                                _ = token.cancelled() => return events.state("idle","Búsqueda cancelada"),
                                result = collect_peers(&airplay) => result,
                            };
                            events.send(json!({"event":"peers","peers":peers}));
                            if !warnings.is_empty() {
                                events.warning(&warnings.join(" · "));
                            }
                            events.state("idle",if peers.is_empty() {
                                "No se encontraron receptores. Comprueba que la TV esté en AirPlay o Compartir pantalla"
                            } else {
                                "Búsqueda terminada. Elige una TV para conectar"
                            });
                        });
                        job_terminal = false;
                        job = Some((cancel,task));
                    }
                    Request::Pin { pin } => {
                        if pin.is_empty() || pin.len() > 128 {
                            events.warning("El PIN o contraseña no es válido");
                        } else if let Err(error) = airplay.pin(&pin).await {
                            events.warning(&format!("No se pudo enviar el PIN: {error:#}"));
                        }
                    }
                    Request::Connect { mac,protocol,ip,port,name,test } if job.is_none() => {
                        let protocol = protocol.unwrap_or_else(|| "miracast".to_string());
                        if protocol == "airplay" {
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
                                    session_events,
                                    cancel.clone(),
                                ));
                                job_terminal = false;
                        job = Some((cancel,task));
                            }
                        } else if protocol == "miracast" {
                            let cancel = CancellationToken::new();
                            let session_events = match session_log::SessionLog::open() {
                                Ok(log) => {
                                    events.send(json!({"event":"log","path":log.path.to_string_lossy()}));
                                    events.with_log(log)
                                },
                                Err(error) => { events.warning(&format!("No se pudo crear un registro privado: {error:#}. La conexión continuará sin registro")); events.clone() }
                            };
                            session_events.trace("session",if test {"pattern"} else {"extended"});
                            let task = tokio::spawn(session::run(mac,test,session_events,cancel.clone()));
                            job_terminal = false;
                        job = Some((cancel,task));
                        } else {
                            events.state("error","Protocolo de envío desconocido");
                        }
                    }
                    _ => events.warning("Ya hay una operación activa. Pulsa Detener antes de iniciar otra"),
                }
            }
        }
    }
    // EOF (widget removal/hot-reload), SIGTERM, Ctrl+C and Quit all take the
    // same cleanup path. Do not abort the session task before it can clean up.
    if let Some((cancel, task)) = job {
        cancel.cancel();
        let _ = timeout(Duration::from_secs(15), task).await;
    }
    airplay.shutdown().await;
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
        Some("self-test") => {
            println!("{}", media::self_test().await?);
            Ok(())
        }
        Some("scan") => {
            let manager = airplay::Manager::new()?;
            let (peers, warnings) = collect_peers(&manager).await;
            manager.shutdown().await;
            println!("{}", json!({"peers":peers,"warnings":warnings}));
            Ok(())
        }
        _ => {
            eprintln!("Uso: omarchy-miracast {{serve|doctor|self-test|scan}}");
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

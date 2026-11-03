use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde_json::json;
use tokio::{
    net::TcpListener,
    time::{Instant, interval, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{
    Events,
    abr::{Controller, LocalPressure},
    display::VirtualDisplay,
    media::{Capture, Source, Video},
    nm::Manager,
    rtcp,
    rtsp::{self, Negotiated},
    send_queue::SendQueue,
};

#[derive(Default)]
struct Resources {
    video: Option<Video>,
    transport: Option<Negotiated>,
    capture: Option<Capture>,
    display: Option<VirtualDisplay>,
    manager: Option<Manager>,
}

impl Resources {
    async fn close(&mut self, events: &Events) {
        // Destroy the encoder before closing its portal fd. This also releases
        // PipeWire buffers before we remove the headless output.
        drop(self.video.take());
        if let Some(mut transport) = self.transport.take() {
            let _ = timeout(Duration::from_millis(800), async {
                transport
                    .wire
                    .request(
                        "SET_PARAMETER",
                        &transport.uri,
                        transport.next_cseq,
                        &[("Session", transport.session.clone())],
                        "wfd_trigger_method: TEARDOWN\r\n",
                    )
                    .await?;
                let message = transport.wire.read().await?;
                if message.method() == "TEARDOWN" {
                    transport.wire.reply(&message, 200, &[], "").await?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await;
        }
        if let Some(capture) = self.capture.take() {
            match timeout(Duration::from_secs(3), capture.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => events.warning(&format!("Cierre del portal: {error:#}")),
                Err(error) => events.warning(&format!("Cierre del portal: {error}")),
            }
        }
        if let Some(manager) = self.manager.take() {
            let wifi_ok = manager.wifi_unchanged().await.unwrap_or(false);
            let _ = manager.close().await;
            if !wifi_ok {
                events.warning("La conexión Wi-Fi habitual cambió durante la prueba. Se canceló Miracast; no se modificará ni reconectará tu red automáticamente.");
            }
        }
        if let Some(display) = self.display.take()
            && let Err(error) = display.remove().await
        {
            events.warning(&format!("No se pudo retirar la salida virtual: {error:#}"));
        }
    }
}

pub async fn run(mac: String, test: bool, events: Events, cancel: CancellationToken) {
    let mut resources = Resources::default();
    let result = tokio::select! {
        _ = cancel.cancelled() => { events.trace("session", "cancelled"); Ok(()) },
        result = connect(&mac,test,&events,&mut resources) => result,
    };
    events.state(
        "stopping",
        "Liberando captura, salida virtual y conexión temporal",
    );
    resources.close(&events).await;
    match result {
        Ok(()) => events.state("idle", "Desconectado"),
        Err(error) => {
            let errno = error.chain().find_map(|e| {
                e.downcast_ref::<std::io::Error>()
                    .and_then(|e| e.raw_os_error())
            });
            events.send(
                json!({"event":"trace","stage":"error","detail":"session_failed","errno":errno}),
            );
            events.state("error", &format!("{error:#}"));
        }
    }
}

async fn supervise<T, Check>(
    period: Duration,
    future: impl Future<Output = Result<T>>,
    mut check: impl FnMut() -> Check,
) -> Result<T>
where
    Check: Future<Output = Result<()>>,
{
    let watchdog = async {
        loop {
            sleep(period).await;
            check().await?;
        }
    };
    tokio::select! { result = future => result, result = watchdog => result }
}

async fn protect_wifi<T>(
    manager: &Manager,
    require_p2p: bool,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    supervise(Duration::from_secs(1),future,|| async {
            ensure!(
                manager.wifi_unchanged().await?,
                "La conexión Wi-Fi habitual cambió. Se cancela Miracast para no seguir usando esta combinación de radios"
            );
            if require_p2p {
                ensure!(manager.network_if_ready().await.context("El enlace P2P dejó de estar disponible durante la sesión Miracast")?.is_some(),
                    "El enlace Wi-Fi Direct dejó de estar activo; no se seguirá esperando un canal TCP de una conexión cerrada");
            }
            Ok(())
    }).await
}

async fn connect(mac: &str, test: bool, events: &Events, resources: &mut Resources) -> Result<()> {
    ensure!(crate::nm::valid_mac(mac), "Dirección de receptor inválida");
    // Listen before advertising the RTSP source. Never answer a connection
    // unless both its local address and peer address belong to our P2P group.
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 7236))
        .await
        .context("TCP 7236 está ocupado por otra aplicación")?;
    resources.manager = Some(Manager::new().await?);
    let peers = resources.manager.as_ref().unwrap().peers(false).await?;
    let peer = peers
        .into_iter()
        .find(|p| p.mac.eq_ignore_ascii_case(mac) && p.wfd)
        .context(
            "La TV ya no está en la lista de NetworkManager. Pulsa Buscar y vuelve a intentarlo",
        )?;

    if !test {
        events.state(
            "display",
            "Creando el monitor extendido a 1920×1080 / 30 Hz",
        );
        resources.display = Some(VirtualDisplay::create().await?);
        let name = &resources.display.as_ref().unwrap().name;
        events.state("capture",&format!("Selecciona {name} en el diálogo de compartir pantalla. No se enviará vídeo antes de tu autorización"));
        resources.capture = Some(
            Capture::new()
                .await
                .context("El portal de captura no está disponible")?,
        );
        let position = resources.display.as_ref().unwrap().position;
        timeout(
            Duration::from_secs(120),
            resources.capture.as_mut().unwrap().select(name, position),
        )
        .await
        .context("Se agotó el tiempo para elegir la pantalla")?
        .context("Captura cancelada o rechazada por el portal")?;
    }

    events.state(
        "p2p",
        &format!(
            "Conectando con {} por Wi-Fi Direct. Acepta la solicitud en la TV",
            peer.name
        ),
    );
    let manager = resources.manager.as_mut().unwrap();
    manager.snapshot_wifi().await?;
    timeout(Duration::from_secs(30), manager.begin(&peer))
        .await
        .context("NetworkManager no respondió a la solicitud P2P")??;
    let network = protect_wifi(manager,false,async {
        timeout(Duration::from_secs(55),async {
            loop {
                if let Some(network) = manager.network_if_ready().await? { return Ok::<_,anyhow::Error>(network); }
                sleep(Duration::from_millis(400)).await;
            }
        }).await.context("Wi-Fi Direct no consiguió formar un grupo con IP en 55 segundos. El problema es anterior a la transmisión de vídeo")?
    }).await?;

    events.state("rtsp","El ordenador tiene IP en el grupo. Aún falta que la TV se incorpore, obtenga IP y abra TCP 7236; no se ha cambiado el firewall");
    let socket = protect_wifi(manager,true,async {
        timeout(Duration::from_secs(35),async {
            loop {
                let (socket,remote) = listener.accept().await?;
                if socket.local_addr()?.ip() != IpAddr::V4(network.address) { continue; }
                if let IpAddr::V4(ip) = remote.ip()
                    && network.contains_peer(ip) {
                    return Ok::<_,anyhow::Error>(socket);
                }
            }
        }).await.context("La TV no abrió TCP 7236 en 35 segundos. Puede haber un bloqueo de UFW o un fallo del receptor; no se abrirán puertos automáticamente")?
    }).await?;
    drop(listener);
    events.state(
        "negotiating",
        "Negociando resolución H.264 y puertos RTP con la TV",
    );
    resources.transport = Some(
        protect_wifi(manager, true, async {
            timeout(Duration::from_secs(45), rtsp::negotiate(socket, events))
                .await
                .context("La negociación Miracast excedió 45 segundos")?
        })
        .await?,
    );
    let transport = resources.transport.as_mut().unwrap();
    if !transport.audio {
        events.warning("Sin audio: la TV no anunció AAC estéreo o faltan componentes AAC. LPCM todavía no está implementado.");
    }
    events.trace(
        "audio",
        if transport.audio {
            "aac_negotiated"
        } else {
            "unavailable"
        },
    );
    events.send(json!({"event":"trace","stage":"video","detail":"negotiated","width":transport.mode.width,"height":transport.mode.height,"fps":transport.mode.fps,"rtcp":transport.rtcp_enabled}));
    let source = if test {
        Source::Test
    } else {
        resources.capture.as_ref().unwrap().source()?
    };
    resources.video = Some(Video::start(
        &transport.mode,
        source,
        crate::abr::INITIAL_KBPS,
        transport.audio,
    )?);
    events.state(
        "starting",
        &format!(
            "Iniciando vídeo {}×{} a {} fps; {}",
            transport.mode.width,
            transport.mode.height,
            transport.mode.fps,
            if transport.audio {
                if test {
                    "tono de prueba AAC"
                } else {
                    "audio AAC de la salida predeterminada"
                }
            } else {
                "sin audio"
            }
        ),
    );
    protect_wifi(
        manager,
        true,
        stream(
            transport,
            resources.video.as_ref().unwrap(),
            events,
            Some(manager),
        ),
    )
    .await
}

async fn stream(
    transport: &mut Negotiated,
    video: &Video,
    events: &Events,
    manager: Option<&Manager>,
) -> Result<()> {
    let mut abr = Controller::default();
    let mut poll = interval(Duration::from_millis(2));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut maintenance = interval(Duration::from_secs(1));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut incoming_rtcp = [0u8; 2048];
    let mut packets = 0u32;
    let mut payload_bytes = 0u32;
    let mut ssrc = 0u32;
    let mut timestamp = 0u32;
    let mut last_packet = Instant::now();
    let mut last_report: Option<Instant> = None;
    let mut last_sequence: Option<u32> = None;
    let mut local_pressure = LocalPressure::new();
    let mut last_keepalive = Instant::now();
    let mut last_sr = Instant::now();
    let mut pending_keepalive: Option<(u32, Instant)> = None;
    let send_timeouts = 0u64; // Kept for compatibility with existing logs.
    let mut send_would_block = 0u64;
    let mut send_memory_errors = 0u64;
    let mut interval_attempts = 0u64;
    let mut interval_failures = 0u64;
    let mut queue = SendQueue::new();
    let mut dropped_packets = 0u64;
    let mut dropped_expired = 0u64;
    let mut dropped_overflow = 0u64;
    let mut queue_peak = 0usize;
    let mut packet_max_bytes = 0usize;
    let mut keyframe_requests = 0u64;
    let mut blocked_since: Option<Instant> = None;
    let mut blocked_max_ms = 0u64;
    let mut last_keyframe = Instant::now() - Duration::from_secs(2);
    let mut keyframe_pending = false;
    let mut retry_send_at = Instant::now();
    loop {
        tokio::select! {
            result = transport.wire.read() => {
                let message = result?;
                if let Some(status) = message.status()? {
                    if pending_keepalive.is_some_and(|(seq,_)| message.cseq().ok() == Some(seq)) {
                        ensure!(status == 200,"La TV rechazó el keepalive con RTSP {status}");
                        pending_keepalive = None;
                    }
                    // An M5 response may arrive after PLAY. Do not treat it as
                    // a response to a later keepalive.
                    continue;
                }
                match message.method() {
                    "GET_PARAMETER" if message.body.trim().is_empty() => transport.wire.reply(&message,200,&[("Session",transport.session.clone())],"").await?,
                    "SET_PARAMETER" if message.body.lines().any(|line| line.trim() == "wfd_idr_request") => {
                        if !rtsp::session_matches(&message,&transport.session) { transport.wire.reply(&message,454,&[],"").await?; continue; }
                        video.request_keyframe();
                        transport.wire.reply(&message,200,&[("Session",transport.session.clone())],"").await?;
                    }
                    "TEARDOWN" => {
                        if !rtsp::session_matches(&message,&transport.session) { transport.wire.reply(&message,454,&[],"").await?; continue; }
                        transport.wire.reply(&message,200,&[],"").await?;
                        return Ok(());
                    }
                    "OPTIONS" => transport.wire.reply(&message,200,&[("Public","OPTIONS, GET_PARAMETER, SET_PARAMETER, TEARDOWN".into())],"").await?,
                    _ => transport.wire.reply(&message,405,&[],"").await?,
                }
            }
            result = transport.udp.rtcp.recv(&mut incoming_rtcp), if transport.rtcp_enabled => {
                let count = result.context("Error al recibir RTCP")?;
                // Count at most one report per second. Duplicate or burst
                // reports must not make recovery jump to the maximum bitrate.
                if packets > 0 && last_report.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
                    && let Some(report) = rtcp::reports(&incoming_rtcp[..count],ssrc).first()
                    && last_sequence.is_none_or(|seq| matches!(report.highest_seq.wrapping_sub(seq),1..=0x7fff_ffff)) {
                    last_report = Some(Instant::now());
                    last_sequence = Some(report.highest_seq);
                    if let Some(kbps) = abr.report(report.loss,report.jitter_ms) { video.set_bitrate(kbps); events.send(json!({"event":"trace","stage":"bitrate","detail":"rtcp_adjustment","kbps":kbps})); }
                }
            }
            _ = poll.tick() => {
                let expired = queue.expire(Instant::now());
                if expired > 0 { dropped_packets += expired; dropped_expired += expired; interval_failures += expired; keyframe_pending = true; }
                for _ in 0..32 {
                    let Some(packet) = video.packet()? else { break };
                    packet_max_bytes = packet_max_bytes.max(packet.len());
                    interval_attempts += 1;
                    if queue.push(Instant::now(),packet) {
                        dropped_packets += 1;
                        dropped_overflow += 1;
                        interval_failures += 1;
                        keyframe_pending = true;
                    }
                    queue_peak = queue_peak.max(queue.len());
                }
                if keyframe_pending && last_keyframe.elapsed() >= Duration::from_secs(2) {
                    video.request_keyframe();
                    keyframe_requests += 1;
                    last_keyframe = Instant::now();
                    keyframe_pending = false;
                }
            }
            ready = async { tokio::time::sleep_until(retry_send_at).await; transport.udp.rtp.writable().await }, if !queue.is_empty() => {
                ready.context("Error al esperar el socket RTP")?;
                let expired = queue.expire(Instant::now());
                if expired > 0 { dropped_packets += expired; dropped_expired += expired; interval_failures += expired; keyframe_pending = true; }
                for _ in 0..32 {
                    let Some(packet) = queue.front() else { break };
                    match transport.udp.rtp.try_send(packet) {
                        Ok(_) => {
                            if let Some(start) = blocked_since.take() { blocked_max_ms = blocked_max_ms.max(start.elapsed().as_millis().min(u64::MAX as u128) as u64); }
                            timestamp = u32::from_be_bytes(packet[4..8].try_into().unwrap());
                            ssrc = u32::from_be_bytes(packet[8..12].try_into().unwrap());
                            payload_bytes = payload_bytes.wrapping_add((packet.len()-12) as u32);
                            packets = packets.wrapping_add(1);
                            queue.pop();
                            last_packet = Instant::now();
                            if packets == 1 { events.state("streaming",if transport.audio { "Enviando vídeo y audio AAC Miracast. Comprueba imagen y sonido en la TV" } else { "Enviando vídeo Miracast sin audio. Comprueba la imagen en la TV" }); }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => { send_would_block += 1; blocked_since.get_or_insert_with(Instant::now); break; }
                        Err(error) if error.kind() == std::io::ErrorKind::OutOfMemory => { send_memory_errors += 1; retry_send_at = Instant::now() + Duration::from_millis(5); break; }
                        Err(error) => return Err(error).context("Falló el envío RTP"),
                    }
                }
            }
            _ = maintenance.tick() => {
                let (captured,encoded,produced) = video.counters();
                if last_packet.elapsed() >= Duration::from_secs(10) {
                    let blocked_current_ms = blocked_since.map_or(0, |start| start.elapsed().as_millis().min(u64::MAX as u128) as u64);
                    events.send(json!({"event":"trace","stage":"stall","detail":"no_packets_sent","captured":captured,"encoded":encoded,"produced":produced,"packets":packets,"queue_dropped":dropped_packets,"queue_expired":dropped_expired,"queue_overflow":dropped_overflow,"queue_len":queue.len(),"queue_peak":queue_peak,"queue_oldest_ms":queue.oldest_age_ms(Instant::now()),"packet_max_bytes":packet_max_bytes,"keyframe_requests":keyframe_requests,"blocked_current_ms":blocked_current_ms,"blocked_max_ms":blocked_max_ms.max(blocked_current_ms),"send_timeouts":send_timeouts,"send_would_block":send_would_block,"send_memory_errors":send_memory_errors}));
                    anyhow::bail!("No se enviaron paquetes de vídeo durante 10 segundos; consulta los contadores de captura, codificación y envío");
                }
                if let Some(manager) = manager {
                    ensure!(manager.network_if_ready().await?.is_some(),"Se perdió el grupo Wi-Fi Direct");
                }
                if let Some((_,sent)) = pending_keepalive {
                    ensure!(sent.elapsed() < Duration::from_secs(12),"La TV dejó de responder a los keepalive RTSP");
                } else if last_keepalive.elapsed() >= Duration::from_secs(10) {
                    let seq = transport.next_cseq;
                    transport.next_cseq += 1;
                    transport.wire.request("GET_PARAMETER",&transport.uri,seq,&[("Session",transport.session.clone())],"").await?;
                    pending_keepalive = Some((seq,Instant::now()));
                    last_keepalive = Instant::now();
                }
                if transport.rtcp_enabled && packets > 0 && last_sr.elapsed() >= Duration::from_secs(3) {
                    let packet = rtcp::sender_report(ssrc,timestamp,packets,payload_bytes);
                    let _ = transport.udp.rtcp.send(&packet).await;
                    last_sr = Instant::now();
                }
                let feedback = last_report.is_some_and(|t| t.elapsed() < Duration::from_secs(10));
                if let Some((reason,kbps)) = local_pressure.observe(interval_attempts,interval_failures,&mut abr,feedback) {
                    video.set_bitrate(kbps);
                    events.send(json!({"event":"trace","stage":"bitrate","detail":reason,"kbps":kbps,"attempts":interval_attempts,"failed":interval_failures}));
                }
                interval_attempts = 0;
                interval_failures = 0;
                let blocked_current_ms = blocked_since.map_or(0, |start| start.elapsed().as_millis().min(u64::MAX as u128) as u64);
                events.send(json!({"event":"metrics","kbps":abr.kbps,"feedback":feedback,"packets":packets,"captured":captured,"encoded":encoded,"produced":produced,"queue_dropped":dropped_packets,"queue_expired":dropped_expired,"queue_overflow":dropped_overflow,"queue_len":queue.len(),"queue_peak":queue_peak,"queue_oldest_ms":queue.oldest_age_ms(Instant::now()),"packet_max_bytes":packet_max_bytes,"keyframe_requests":keyframe_requests,"blocked_current_ms":blocked_current_ms,"blocked_max_ms":blocked_max_ms.max(blocked_current_ms),"send_timeouts":send_timeouts,"send_would_block":send_would_block,"send_memory_errors":send_memory_errors,"width":transport.mode.width,"height":transport.mode.height,"fps":transport.mode.fps}));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::{UdpPair, Wire};

    #[tokio::test]
    async fn lost_link_ends_wait_before_generic_tcp_timeout() {
        let result = timeout(
            Duration::from_secs(1),
            supervise(
                Duration::from_millis(5),
                std::future::pending::<Result<()>>(),
                || std::future::ready(Err(anyhow::anyhow!("P2P cerrada"))),
            ),
        )
        .await
        .expect("El supervisor no detectó la caída del enlace");
        assert!(result.unwrap_err().to_string().contains("P2P cerrada"));
    }

    #[tokio::test]
    async fn completed_operation_does_not_wait_for_watchdog() {
        let result = supervise(Duration::from_secs(10), std::future::ready(Ok(42)), || {
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();
        assert_eq!(result, 42);
    }
    use gstreamer::{self as gst, prelude::*};
    use gstreamer_app::AppSrc;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::net::TcpStream;

    struct Decoder {
        pipeline: gst::Pipeline,
        input: AppSrc,
        frames: Arc<AtomicUsize>,
        audio: Arc<AtomicUsize>,
    }

    impl Decoder {
        fn new() -> Result<Self> {
            let pipeline = gst::parse::launch(
                "appsrc name=input is-live=true format=time do-timestamp=true \
                 caps=\"application/x-rtp,media=(string)video,clock-rate=(int)90000,encoding-name=(string)MP2T,payload=(int)33\" \
                 ! rtpmp2tdepay ! tsdemux name=demux queue name=video_queue ! h264parse ! avdec_h264 \
                 ! fakesink name=decoded sync=false async=false signal-handoffs=true \
                 queue name=audio_queue ! aacparse ! avdec_aac ! fakesink name=decoded_audio sync=false async=false signal-handoffs=true")?
                .downcast::<gst::Pipeline>().map_err(|_|anyhow::anyhow!("decoder pipeline"))?;
            let vpad = pipeline
                .by_name("video_queue")
                .unwrap()
                .static_pad("sink")
                .unwrap();
            let apad = pipeline
                .by_name("audio_queue")
                .unwrap()
                .static_pad("sink")
                .unwrap();
            pipeline
                .by_name("demux")
                .unwrap()
                .connect_pad_added(move |_, pad| {
                    let target = if pad.name().starts_with("video_") {
                        &vpad
                    } else if pad.name().starts_with("audio_") {
                        &apad
                    } else {
                        return;
                    };
                    let _ = pad.link(target);
                });
            let input = pipeline
                .by_name("input")
                .unwrap()
                .downcast::<AppSrc>()
                .unwrap();
            let frames = Arc::new(AtomicUsize::new(0));
            let audio = Arc::new(AtomicUsize::new(0));
            let audio_counter = audio.clone();
            pipeline
                .by_name("decoded_audio")
                .unwrap()
                .connect("handoff", false, move |_| {
                    audio_counter.fetch_add(1, Ordering::Relaxed);
                    None
                });
            let counter = frames.clone();
            pipeline
                .by_name("decoded")
                .unwrap()
                .connect("handoff", false, move |_| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    None
                });
            pipeline.set_state(gst::State::Playing)?;
            Ok(Self {
                pipeline,
                input,
                frames,
                audio,
            })
        }
    }
    impl Drop for Decoder {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    #[tokio::test]
    async fn simulated_sink_negotiates_decodes_adapts_and_tears_down() {
        gst::init().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        let events = Events(tx, None);
        let source = async {
            let (socket, _) = listener.accept().await?;
            let mut session = rtsp::negotiate(socket, &events).await?;
            ensure!(
                (session.mode.width, session.mode.height, session.mode.fps) == (1920, 1080, 30),
                "Wrong negotiated mode"
            );
            ensure!(session.audio, "Audio AAC no negociado");
            let video = Video::start(&session.mode, Source::Test, 4000, session.audio)?;
            stream(&mut session, &video, &events, None).await?;
            ensure!(
                video.bitrate() == 3000,
                "The encoder did not adapt live: {}",
                video.bitrate()
            );
            Ok::<(), anyhow::Error>(())
        };
        let sink = async {
            let mut wire = Wire::new(TcpStream::connect(address).await?);
            let udp = UdpPair::bind(Ipv4Addr::LOCALHOST).await?;
            let m1 = wire.read().await?;
            ensure!(m1.method() == "OPTIONS", "Missing M1");
            // Exercise simultaneous requests and reversed M1/M2 ordering.
            wire.request("OPTIONS", "*", 100, &[], "").await?;
            wire.reply(
                &m1,
                200,
                &[(
                    "Public",
                    "org.wfa.wfd1.0, OPTIONS, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, TEARDOWN"
                        .into(),
                )],
                "",
            )
            .await?;
            ensure!(wire.read().await?.cseq()? == 100, "Missing M2 response");
            let m3 = wire.read().await?;
            ensure!(m3.method() == "GET_PARAMETER", "Missing M3");
            wire.reply(&m3,200,&[],&format!(
                "wfd_video_formats: 00 00 02 1f ffffffff ffffffff 00000000 00 0000 0000 00 none none\r\n\
                 wfd_audio_codecs: LPCM 00000003 00, AAC 00000001 00\r\n\
                 wfd_client_rtp_ports: RTP/AVP/UDP;unicast {} 0 mode=play\r\n\
                 wfd_content_protection: none\r\n",udp.port)).await?;
            let m4 = wire.read().await?;
            ensure!(
                m4.body.contains("00000080")
                    && m4.body.contains("wfd_audio_codecs: AAC 00000001 00"),
                "Wrong M4"
            );
            wire.reply(&m4, 200, &[], "").await?;
            let m5 = wire.read().await?;
            ensure!(m5.body.contains("wfd_trigger_method: SETUP"), "Missing M5");
            // SETUP and PLAY are intentionally sent before acknowledging M5.
            wire.request(
                "SETUP",
                "rtsp://127.0.0.1/wfd1.0/streamid=0",
                101,
                &[(
                    "Transport",
                    format!(
                        "RTP/AVP/UDP;unicast;client_port={}-{}",
                        udp.port,
                        udp.port + 1
                    ),
                )],
                "",
            )
            .await?;
            let setup = wire.read().await?;
            ensure!(setup.status()? == Some(200), "SETUP failed");
            let session_id = setup.headers["session"]
                .split(';')
                .next()
                .unwrap()
                .to_string();
            let server_ports = setup.headers["transport"]
                .split(';')
                .find_map(|s| s.strip_prefix("server_port="))
                .unwrap();
            let server_rtcp: u16 = server_ports.split_once('-').unwrap().1.parse()?;
            udp.rtcp.connect((Ipv4Addr::LOCALHOST, server_rtcp)).await?;
            wire.request(
                "PLAY",
                "rtsp://127.0.0.1/wfd1.0/streamid=0",
                102,
                &[("Session", session_id.clone())],
                "",
            )
            .await?;
            ensure!(wire.read().await?.status()? == Some(200), "PLAY failed");
            wire.reply(&m5, 200, &[], "").await?;
            let decoder = Decoder::new()?;
            let start = Instant::now();
            let end = start + Duration::from_millis(3300);
            let mut packet = [0u8; 2048];
            let mut reports = 0;
            let mut received = 0u32;
            while Instant::now() < end {
                let count = timeout(Duration::from_secs(2), udp.rtp.recv(&mut packet)).await??;
                ensure!(count >= 12 && packet[1] & 0x7f == 33, "Invalid RTP");
                received += 1;
                // Join after the mux has started both elementary streams, as a
                // late-joining receiver would. This also exercises repeated PAT/PMT.
                if start.elapsed() > Duration::from_millis(600) {
                    decoder
                        .input
                        .push_buffer(gst::Buffer::from_mut_slice(packet[..count].to_vec()))?;
                }
                let report_due = if reports == 0 {
                    Duration::from_millis(300)
                } else {
                    Duration::from_millis(1500)
                };
                if reports < 2 && start.elapsed() > report_due {
                    let mut rr = vec![0x81, 201, 0, 7];
                    rr.extend(123u32.to_be_bytes());
                    rr.extend_from_slice(&packet[8..12]);
                    rr.extend([32, 0, 0, 0]); // 12.5% synthetic loss
                    rr.extend(received.to_be_bytes());
                    rr.extend(0u32.to_be_bytes());
                    rr.extend([0; 8]);
                    udp.rtcp.send(&rr).await?;
                    reports += 1;
                }
            }
            ensure!(reports == 2, "Missing synthetic receiver reports");
            ensure!(
                decoder.frames.load(Ordering::Relaxed) >= 15,
                "No decodable H.264 frames received"
            );
            ensure!(
                decoder.audio.load(Ordering::Relaxed) >= 15,
                "No decodable AAC audio received"
            );
            wire.request(
                "TEARDOWN",
                "rtsp://127.0.0.1/wfd1.0/streamid=0",
                103,
                &[("Session", session_id)],
                "",
            )
            .await?;
            ensure!(wire.read().await?.status()? == Some(200), "TEARDOWN failed");
            Ok::<(), anyhow::Error>(())
        };
        let result = timeout(Duration::from_secs(12), async {
            tokio::try_join!(source, sink)
        })
        .await;
        result.unwrap().unwrap();
        let mut adapted = false;
        while let Ok(event) = rx.try_recv() {
            if event["event"] == "metrics" && event["kbps"] == 3000 && event["feedback"] == true {
                adapted = true;
            }
        }
        assert!(
            adapted,
            "The UI never received the adaptive bitrate metrics"
        );
    }
}

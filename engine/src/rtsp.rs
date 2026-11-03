use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    time::timeout,
};

use crate::wfd::{self, VideoMode};

const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;

#[derive(Debug)]
pub struct Message {
    pub first: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Message {
    pub fn cseq(&self) -> Result<u32> {
        self.headers
            .get("cseq")
            .context("RTSP sin CSeq")?
            .parse()
            .context("CSeq RTSP inválido")
    }

    pub fn status(&self) -> Result<Option<u16>> {
        if self.first.starts_with("RTSP/1.0 ") {
            Ok(Some(
                self.first
                    .split_whitespace()
                    .nth(1)
                    .context("Respuesta RTSP inválida")?
                    .parse()?,
            ))
        } else {
            ensure!(
                self.first.ends_with(" RTSP/1.0"),
                "Versión RTSP no compatible"
            );
            Ok(None)
        }
    }

    pub fn method(&self) -> &str {
        self.first.split_whitespace().next().unwrap_or("")
    }
}

fn decode(buffer: &mut Vec<u8>) -> Result<Option<Message>> {
    let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
        ensure!(buffer.len() <= MAX_HEADER, "Cabecera RTSP demasiado grande");
        return Ok(None);
    };
    ensure!(end <= MAX_HEADER, "Cabecera RTSP demasiado grande");
    let header = std::str::from_utf8(&buffer[..end]).context("Cabecera RTSP no UTF-8")?;
    let mut lines = header.split("\r\n");
    let first = lines.next().context("RTSP vacío")?.to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').context("Cabecera RTSP malformada")?;
        let name = name.to_ascii_lowercase();
        ensure!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Nombre de cabecera inválido"
        );
        ensure!(!headers.contains_key(&name), "Cabecera RTSP duplicada");
        headers.insert(name, value.trim().to_owned());
    }
    let length: usize = headers
        .get("content-length")
        .map_or(Ok(0), |v| v.parse())
        .context("Content-Length inválido")?;
    ensure!(length <= MAX_BODY, "Cuerpo RTSP demasiado grande");
    let total = end + 4 + length;
    if buffer.len() < total {
        return Ok(None);
    }
    let body = std::str::from_utf8(&buffer[end + 4..total])
        .context("Parámetros RTSP no UTF-8")?
        .to_owned();
    buffer.drain(..total);
    let message = Message {
        first,
        headers,
        body,
    };
    message.cseq()?;
    message.status()?;
    Ok(Some(message))
}

pub struct Wire {
    socket: TcpStream,
    buffer: Vec<u8>,
}

impl Wire {
    pub fn new(socket: TcpStream) -> Self {
        Self {
            socket,
            buffer: Vec::new(),
        }
    }

    // Cancellation-safe: incomplete frames remain in `buffer` across select!.
    pub async fn read(&mut self) -> Result<Message> {
        loop {
            if let Some(message) = decode(&mut self.buffer)? {
                return Ok(message);
            }
            let mut chunk = [0u8; 4096];
            let count = self.socket.read(&mut chunk).await?;
            ensure!(count > 0, "La TV cerró el canal RTSP");
            self.buffer.extend_from_slice(&chunk[..count]);
        }
    }

    async fn send(
        &mut self,
        first: &str,
        cseq: u32,
        headers: &[(&str, String)],
        body: &str,
    ) -> Result<()> {
        let mut output = format!("{first}\r\nCSeq: {cseq}\r\nServer: OmarchyMiracast/0.1\r\n");
        for (key, value) in headers {
            output.push_str(&format!("{key}: {value}\r\n"));
        }
        if !body.is_empty() {
            output.push_str("Content-Type: text/parameters\r\n");
        }
        output.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        timeout(
            Duration::from_secs(4),
            self.socket.write_all(output.as_bytes()),
        )
        .await
        .context("La TV dejó de leer RTSP")??;
        Ok(())
    }

    pub async fn request(
        &mut self,
        method: &str,
        uri: &str,
        cseq: u32,
        headers: &[(&str, String)],
        body: &str,
    ) -> Result<()> {
        self.send(&format!("{method} {uri} RTSP/1.0"), cseq, headers, body)
            .await
    }

    pub async fn reply(
        &mut self,
        message: &Message,
        status: u16,
        headers: &[(&str, String)],
        body: &str,
    ) -> Result<()> {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            405 => "Method Not Allowed",
            454 => "Session Not Found",
            455 => "Method Not Valid in This State",
            461 => "Unsupported Transport",
            _ => "Error",
        };
        self.send(
            &format!("RTSP/1.0 {status} {reason}"),
            message.cseq()?,
            headers,
            body,
        )
        .await
    }
}

pub struct UdpPair {
    pub rtp: UdpSocket,
    pub rtcp: UdpSocket,
    pub port: u16,
}

impl UdpPair {
    pub async fn bind(local: Ipv4Addr) -> Result<Self> {
        for _ in 0..64 {
            let rtp = UdpSocket::bind((local, 0)).await?;
            let port = rtp.local_addr()?.port();
            if port % 2 != 0 || port == 65535 {
                continue;
            }
            if let Ok(rtcp) = UdpSocket::bind((local, port + 1)).await {
                return Ok(Self { rtp, rtcp, port });
            }
        }
        bail!("No hay un par de puertos UDP consecutivos disponible")
    }
}

pub struct Negotiated {
    pub wire: Wire,
    pub mode: VideoMode,
    pub audio: bool,
    pub udp: UdpPair,
    pub session: String,
    pub next_cseq: u32,
    pub rtcp_enabled: bool,
    pub uri: String,
}

/// The sink connects to the source, but the source initiates M1/M3/M4/M5.
/// Requests from the sink are serviced even while our own request is pending.
pub async fn negotiate(socket: TcpStream, events: &crate::Events) -> Result<Negotiated> {
    socket.set_nodelay(true)?;
    let local = match socket.local_addr()?.ip() {
        std::net::IpAddr::V4(ip) => ip,
        _ => bail!("Solo IPv4 en esta versión"),
    };
    let peer = match socket.peer_addr()?.ip() {
        std::net::IpAddr::V4(ip) => ip,
        _ => bail!("Solo IPv4 en esta versión"),
    };
    let udp = UdpPair::bind(local).await?;
    let session = format!("{:x}{:x}", std::process::id(), udp.port);
    let uri = format!("rtsp://{local}/wfd1.0/streamid=0");
    let mut wire = Wire::new(socket);
    wire.request(
        "OPTIONS",
        "*",
        1,
        &[("Require", "org.wfa.wfd1.0".into())],
        "",
    )
    .await?;
    events.trace("rtsp", "options_sent");
    let mut options_response = false;
    let mut sink_options = false;
    let mut pending = (1, "options");
    let mut next_cseq = 2;
    let mut mode = None;
    let mut audio = false;
    let mut capability_port = None;
    let mut triggered = false;
    let mut setup = false;
    let mut rtcp_enabled = false;
    loop {
        let message = timeout(Duration::from_secs(15), wire.read())
            .await
            .context("La TV no completó la negociación RTSP en 15 segundos")??;
        if let Some(status) = message.status()? {
            events.trace("rtsp_response", &format!("{}:{status}", pending.1));
            ensure!(
                status == 200,
                "La TV rechazó {} con RTSP {status}; HDCP y algunas extensiones no están implementados",
                pending.1
            );
            ensure!(
                message.cseq()? == pending.0,
                "Respuesta RTSP fuera de secuencia"
            );
            match pending.1 {
                "options" => options_response = true,
                "capabilities" => {
                    let selected = wfd::select_video(
                        wfd::parameter(&message.body, "wfd_video_formats")
                            .context("La TV no respondió wfd_video_formats")?,
                    )?;
                    let port = wfd::rtp_port(
                        wfd::parameter(&message.body, "wfd_client_rtp_ports")
                            .context("La TV no respondió wfd_client_rtp_ports")?,
                    )?;
                    audio = wfd::parameter(&message.body, "wfd_audio_codecs")
                        .is_some_and(wfd::supports_aac)
                        && crate::media::audio_available();
                    let audio_parameter = if audio { "AAC 00000001 00" } else { "none" };
                    let body = format!(
                        "wfd_video_formats: {}\r\nwfd_audio_codecs: {audio_parameter}\r\nwfd_presentation_URL: {uri} none\r\nwfd_client_rtp_ports: RTP/AVP/UDP;unicast {port} 0 mode=play\r\nwfd_content_protection: none\r\n",
                        selected.parameter()
                    );
                    mode = Some(selected);
                    capability_port = Some(port);
                    wire.request(
                        "SET_PARAMETER",
                        "rtsp://localhost/wfd1.0",
                        next_cseq,
                        &[],
                        &body,
                    )
                    .await?;
                    pending = (next_cseq, "parameters");
                    next_cseq += 1;
                }
                "parameters" => {
                    triggered = true;
                    wire.request(
                        "SET_PARAMETER",
                        "rtsp://localhost/wfd1.0",
                        next_cseq,
                        &[],
                        "wfd_trigger_method: SETUP\r\n",
                    )
                    .await?;
                    pending = (next_cseq, "trigger");
                    next_cseq += 1;
                }
                "trigger" => pending = (0, "play"),
                _ => bail!("Respuesta RTSP inesperada"),
            }
        } else {
            match message.method() {
                "OPTIONS" => {
                    events.trace("rtsp", "sink_options");
                    wire.reply(&message,200,&[("Public","org.wfa.wfd1.0, OPTIONS, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, TEARDOWN".into())],"").await?;
                    sink_options = true;
                }
                "GET_PARAMETER" if message.body.trim().is_empty() => {
                    wire.reply(&message, 200, &[], "").await?
                }
                "SETUP" if triggered && !setup => {
                    events.trace("rtsp", "setup_received");
                    let transport = message
                        .headers
                        .get("transport")
                        .context("SETUP sin Transport")?;
                    let (port, rtcp) = match wfd::client_ports(transport) {
                        Ok(value) => value,
                        Err(error) => {
                            wire.reply(&message, 461, &[], "").await?;
                            return Err(error);
                        }
                    };
                    ensure!(
                        Some(port) == capability_port,
                        "La TV cambió el puerto RTP respecto a sus capacidades"
                    );
                    udp.rtp.connect(SocketAddr::from((peer, port))).await?;
                    if let Some(port) = rtcp {
                        udp.rtcp.connect(SocketAddr::from((peer, port))).await?;
                        rtcp_enabled = true;
                    }
                    let client_ports = if rtcp_enabled {
                        format!("{port}-{}", port + 1)
                    } else {
                        port.to_string()
                    };
                    let server_ports = if rtcp_enabled {
                        format!("{}-{}", udp.port, udp.port + 1)
                    } else {
                        udp.port.to_string()
                    };
                    wire.reply(&message,200,&[
                        ("Session",format!("{session};timeout=30")),
                        ("Transport",format!("RTP/AVP/UDP;unicast;client_port={client_ports};server_port={server_ports}")),
                    ],"").await?;
                    setup = true;
                }
                "PLAY" if setup => {
                    if !session_matches(&message, &session) {
                        wire.reply(&message, 454, &[], "").await?;
                        continue;
                    }
                    wire.reply(&message, 200, &[("Session", session.clone())], "")
                        .await?;
                    events.trace("rtsp", "play_accepted");
                    return Ok(Negotiated {
                        wire,
                        mode: mode.context("No se negoció vídeo")?,
                        audio,
                        udp,
                        session,
                        next_cseq,
                        rtcp_enabled,
                        uri,
                    });
                }
                "TEARDOWN" => {
                    wire.reply(&message, 200, &[], "").await?;
                    bail!("La TV canceló la sesión");
                }
                _ => wire.reply(&message, 455, &[], "").await?,
            }
        }
        if sink_options && options_response && pending.1 == "options" {
            wire.request("GET_PARAMETER","rtsp://localhost/wfd1.0",next_cseq,&[],"wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\nwfd_content_protection\r\n").await?;
            pending = (next_cseq, "capabilities");
            next_cseq += 1;
        }
    }
}

pub fn session_matches(message: &Message, session: &str) -> bool {
    message
        .headers
        .get("session")
        .and_then(|s| s.split(';').next())
        .is_some_and(|s| s.trim() == session)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_and_pipelined_messages_are_preserved() {
        let first = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 4\r\n\r\ntest";
        for cut in 0..first.len() {
            let mut buffer = first[..cut].to_vec();
            assert!(decode(&mut buffer).unwrap().is_none());
        }
        let mut buffer = first.repeat(2);
        assert_eq!(decode(&mut buffer).unwrap().unwrap().body, "test");
        assert_eq!(buffer, first);
        assert_eq!(decode(&mut buffer).unwrap().unwrap().cseq().unwrap(), 1);
        assert!(buffer.is_empty());
    }
    #[test]
    fn rejects_unbounded_or_ambiguous_messages() {
        for message in [
            "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 10000000\r\n\r\n",
            "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\n",
            "OPTIONS * RTSP/1.0\r\nCSeq: invalid\r\n\r\n",
        ] {
            assert!(decode(&mut message.as_bytes().to_vec()).is_err());
        }
        assert!(decode(&mut vec![b'x'; MAX_HEADER + 1]).is_err());
    }
}

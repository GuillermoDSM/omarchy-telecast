//! NTP/HAP mirroring session for an already authenticated connection.
//!
//! Receivers without FPSAP use pair-verify control keys as stream material.
//! FairPlay material, if present, must come from SAP on the same connection.
use super::{
    event,
    pairing::derive,
    rtsp::Client,
    timing,
    transport::{AudioAad, AudioSender, VideoSender},
};
use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};
use rand_core::{OsRng, RngCore};
use std::{
    io::Cursor,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Cryptographic output of the FairPlay exchange performed on this connection.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct FairPlayMaterial {
    pub ekey: Vec<u8>,
    pub iv: [u8; 16],
    pub master_key: [u8; 16],
}

pub struct Config {
    pub peer: SocketAddr,
    pub session_uuid: String,
    pub device_id: String,
    pub name: String,
    pub audio_latency_samples: u32,
    pub video_latency_ms: u32,
    pub stream_connections: bool,
    pub audio_aad: AudioAad,
    pub receiver_features: u64,
    pub timing: TimingMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimingMode {
    Ntp,
    Ptp,
}
/// The receiver advertised PTP but its SETUP named no clock timeline. DoubleTake
/// reconnects once over NTP for these receivers (LG webOS 5, Samsung), which is
/// also required here.
#[derive(Debug)]
pub struct MissingReceiverTimeline;
impl std::fmt::Display for MissingReceiverTimeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("receiver advertised PTP but named no timeline")
    }
}
impl std::error::Error for MissingReceiverTimeline {}

#[derive(Debug)]
struct SetupStatus(u16);
impl std::fmt::Display for SetupStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SETUP rejected: {}", self.0)
    }
}
impl std::error::Error for SetupStatus {}

/// Owns transports and timing/event tasks until `stop` or drop.
pub struct Session {
    control: Client,
    uri: String,
    session_uuid: String,
    pub video: Option<VideoSender>,
    pub audio: AudioSender,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    clock: Option<timing::MediaClock>,
    pub video_latency_ms: u32,
    pub audio_latency_samples: u32,
}

fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<Dictionary>(),
    )
}
fn random_id() -> u64 {
    OsRng.next_u64() & i64::MAX as u64
}
fn endpoint(ip: IpAddr, port: u16) -> SocketAddr {
    SocketAddr::new(ip, port)
}
fn wildcard(ip: IpAddr) -> SocketAddr {
    endpoint(
        if ip.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        },
        0,
    )
}

/// DoubleTake allocates timing, RTCP and RTP on consecutive ports. Mirror that
/// so receivers that assume the layout accept the media session.
async fn consecutive_udp(ip: IpAddr) -> Result<(UdpSocket, UdpSocket, UdpSocket)> {
    for _ in 0..20 {
        let timing = UdpSocket::bind(wildcard(ip)).await?;
        let base = u32::from(timing.local_addr()?.port());
        if base > u32::from(u16::MAX) - 2 {
            continue;
        }
        let bind = |port| {
            let address = wildcard(ip);
            endpoint(address.ip(), port)
        };
        let control = match UdpSocket::bind(bind((base + 1) as u16)).await {
            Ok(socket) => socket,
            Err(_) => continue,
        };
        let data = match UdpSocket::bind(bind((base + 2) as u16)).await {
            Ok(socket) => socket,
            Err(_) => continue,
        };
        return Ok((timing, control, data));
    }
    anyhow::bail!("no se encontraron tres puertos UDP consecutivos")
}

async fn setup_full(
    client: &mut Client,
    uri: &str,
    session: &str,
    value: Value,
) -> Result<(Value, std::collections::BTreeMap<String, String>)> {
    let mut body = Vec::new();
    value.to_writer_binary(&mut body)?;
    let reply = client
        .request(
            "SETUP",
            uri,
            &[
                ("Content-Type", "application/x-apple-binary-plist"),
                ("Session", session),
            ],
            &body,
        )
        .await?;
    if !(200..300).contains(&reply.status) {
        return Err(SetupStatus(reply.status).into());
    }
    Ok((
        Value::from_reader(Cursor::new(reply.body)).context("invalid SETUP plist")?,
        reply.headers,
    ))
}
async fn setup(client: &mut Client, uri: &str, session: &str, value: Value) -> Result<Value> {
    Ok(setup_full(client, uri, session, value).await?.0)
}
fn port(root: &Value, kind: u64, name: &str) -> Result<u16> {
    let entries = root
        .as_dictionary()
        .and_then(|d| d.get("streams"))
        .and_then(Value::as_array)
        .context("SETUP omitted streams")?;
    let stream = entries
        .iter()
        .filter_map(Value::as_dictionary)
        .find(|s| s.get("type").and_then(Value::as_unsigned_integer) == Some(kind))
        .context("SETUP omitted requested stream")?;
    let kind = match name {
        "dataPort" => "streamConnectionTypeRTP",
        "controlPort" => "streamConnectionTypeRTCP",
        _ => "",
    };
    let nested = stream
        .get("streamConnections")
        .and_then(Value::as_dictionary)
        .and_then(|d| d.get(kind))
        .and_then(Value::as_dictionary)
        .and_then(|d| d.get("streamConnectionKeyPort"))
        .and_then(Value::as_unsigned_integer);
    let value = nested
        .or_else(|| stream.get(name).and_then(Value::as_unsigned_integer))
        .context(format!("SETUP omitted {name}"))?;
    let result = u16::try_from(value)?;
    ensure!(result > 0, "invalid {name}");
    Ok(result)
}
async fn teardown(client: &mut Client, uri: &str, session: &str) -> Result<()> {
    let response = client
        .request("TEARDOWN", uri, &[("Session", session)], &[])
        .await?;
    ensure!(
        (200..300).contains(&response.status),
        "TEARDOWN rejected: {}",
        response.status
    );
    Ok(())
}

impl Session {
    pub fn handle_event(&mut self, event: &event::Event) -> Result<()> {
        if event.method != "POST" || event.path != "/command" || event.body.is_empty() {
            return Ok(());
        }
        let value = Value::from_reader(Cursor::new(&event.body)).context("invalid event plist")?;
        let command = value
            .as_dictionary()
            .context("event command is not a dictionary")?;
        if command.get("type").and_then(Value::as_string) == Some("updateTimingPeerInfo") {
            let id = command
                .get("value")
                .and_then(Value::as_dictionary)
                .and_then(|v| v.get("ClockID"))
                .and_then(Value::as_unsigned_integer)
                .context("timing update omitted ClockID")?;
            if let Some(clock) = &mut self.clock {
                clock.update_timeline(std::time::Instant::now(), id)?;
            }
        }
        Ok(())
    }
    pub fn network_time(&self, at: std::time::Instant) -> Result<(u64, u64)> {
        match &self.clock {
            Some(clock) => Ok((
                clock.at(at).context("PTP clock out of range")?,
                clock.timeline_id,
            )),
            None => {
                let now = std::time::Instant::now();
                let boot = timing::boot_time()?;
                let capture = if at <= now {
                    boot.checked_sub(now.duration_since(at))
                } else {
                    boot.checked_add(at.duration_since(now))
                }
                .context("NTP capture time out of range")?;
                Ok((timing::compact_time(capture), 0))
            }
        }
    }
    /// Move the video transport into its own task so TCP backpressure cannot
    /// delay audio pacing.
    pub fn take_video(&mut self) -> Result<VideoSender> {
        self.video.take().context("video transport already taken")
    }

    /// Send exactly 352 interleaved stereo S16LE samples. The caller owns
    /// pacing and advances the RTP timestamp by 352 for every frame.
    pub async fn send_pcm(&mut self, pcm: &[u8], rtp_timestamp: u32) -> Result<()> {
        let packet = super::codec::alac_stereo(pcm)?;
        self.audio.send(&packet, rtp_timestamp, 0x60).await
    }
    /// Executes SETUP/RECORD after HAP pair-verify. `None` is only valid when
    /// the receiver does not advertise FPSAP (feature bit 14).
    /// No success is returned until every accepted stream has a valid endpoint.
    pub async fn start(
        mut control: Client,
        config: Config,
        shared: [u8; 32],
        fairplay: Option<FairPlayMaterial>,
        events: mpsc::Sender<event::Event>,
    ) -> Result<Self> {
        ensure!(
            control.is_encrypted(),
            "SETUP requires an authenticated encrypted control channel"
        );
        if let Some(fp) = &fairplay {
            ensure!(
                fp.ekey.len() == 72 && &fp.ekey[..4] == b"FPLY",
                "invalid FairPlay key record"
            );
        }
        ensure!(
            fairplay.is_some() || config.receiver_features & (1 << 14) == 0,
            "receiver advertises FPSAP; FairPlay handshake is required"
        );
        let control_write = derive(&shared, b"Control-Salt", b"Control-Write-Encryption-Key");
        let control_read = derive(&shared, b"Control-Salt", b"Control-Read-Encryption-Key");
        let video_master = fairplay
            .as_ref()
            .map(|f| f.master_key)
            .unwrap_or_else(|| control_write[..16].try_into().unwrap());
        let video_iv = fairplay
            .as_ref()
            .map(|f| f.iv)
            .unwrap_or_else(|| control_read[..16].try_into().unwrap());
        ensure!(
            !config.session_uuid.is_empty() && !config.device_id.is_empty(),
            "missing session identity"
        );
        let (timing_socket, audio_control, audio_data) = consecutive_udp(config.peer.ip()).await?;
        let timing_socket = Arc::new(timing_socket);
        let timing_port = timing_socket.local_addr()?.port();
        let cancel = CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let worker_cancel = cancel.clone();
        let peer = config.peer;
        let mut tasks = Vec::new();
        if config.timing == TimingMode::Ntp {
            let socket = timing_socket.clone();
            tasks.push(tokio::spawn(async move {
                let _ = timing::serve_timing(&socket, peer, worker_cancel).await;
            }));
        }
        let audio_id = random_id();
        let video_id = random_id();
        let audio_uri = format!("rtsp://{}/{}", config.peer, audio_id);
        let video_uri = format!("rtsp://{}/{}", config.peer, video_id);
        let mut accepted = false;
        let result = async {
            let mut root = dict(vec![
                ("deviceID", config.device_id.as_str().into()),
                ("macAddress", config.device_id.as_str().into()),
                ("sessionUUID", config.session_uuid.as_str().into()),
                ("sourceVersion", "980.71.1".into()),
                ("isScreenMirroringSession", true.into()),
                (
                    "timingProtocol",
                    if config.timing == TimingMode::Ntp {
                        "NTP"
                    } else {
                        "PTP"
                    }
                    .into(),
                ),
                ("osBuildVersion", "13F69".into()),
                ("model", "Linux".into()),
                ("name", config.name.as_str().into()),
                ("updateSessionRequest", false.into()),
                ("combinedGetInfoWithControlSetup", true.into()),
            ]);
            let root_dict = root.as_dictionary_mut().unwrap();
            if config.timing == TimingMode::Ntp {
                root_dict.insert("timingPort".into(), timing_port.into());
            } else {
                let local = control.local_addr()?.ip().to_string();
                let peer_info = dict(vec![
                    ("ID", config.session_uuid.as_str().into()),
                    ("SupportsClockPortMatchingOverride", true.into()),
                    ("DeviceType", 0u64.into()),
                    ("Addresses", vec![local.into()].into()),
                ]);
                root_dict.insert("timingPeerInfo".into(), peer_info.clone());
                root_dict.insert("timingPeerList".into(), vec![peer_info].into());
            }
            let control_result =
                setup_full(&mut control, &audio_uri, &config.session_uuid, root.clone())
                    .await
                    .context("control SETUP");
            let first = match control_result {
                Ok(value) => {
                    accepted = true;
                    Some((value, std::time::Instant::now()))
                }
                Err(error)
                    if error
                        .downcast_ref::<SetupStatus>()
                        .is_some_and(|s| matches!(s.0, 400 | 405 | 406 | 415 | 455 | 501)) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            let mut timing_probes = false;
            let receiver_timing_port = first.as_ref().and_then(|pair| {
                pair.0
                    .0
                    .as_dictionary()
                    .and_then(|response| response.get("timingPort"))
                    .and_then(Value::as_unsigned_integer)
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port != 0)
            });
            if config.timing == TimingMode::Ntp
                && let Some(port) = receiver_timing_port
            {
                timing::send_probes(&timing_socket, endpoint(peer.ip(), port)).await?;
                timing_probes = true;
            }
            let mut audio_key = [0; 32];
            OsRng.fill_bytes(&mut audio_key);
            let mut audio = Dictionary::new();
            for (key, value) in [
                ("type", 96u64.into()),
                ("streamConnectionID", audio_id.into()),
                ("ct", 2u64.into()),
                ("spf", 352u64.into()),
                ("sr", 44100u64.into()),
                ("audioFormat", 0x40000u64.into()),
                ("redundantAudio", 2u64.into()),
                ("audioMode", "default".into()),
                ("usingScreen", true.into()),
                ("latencyMin", 0u64.into()),
                ("latencyMax", config.audio_latency_samples.into()),
                ("shk", Value::Data(audio_key.to_vec())),
            ] {
                audio.insert(key.into(), value);
            }
            if config.stream_connections {
                audio.insert("isMedia".into(), false.into());
                audio.insert("supportsDynamicStreamID".into(), true.into());
                audio.insert(
                    "streamConnections".into(),
                    dict(vec![
                        (
                            "streamConnectionTypeRTP",
                            dict(vec![(
                                "streamConnectionKeyUseStreamEncryptionKey",
                                true.into(),
                            )]),
                        ),
                        (
                            "streamConnectionTypeRTCP",
                            dict(vec![(
                                "streamConnectionKeyPort",
                                audio_control.local_addr()?.port().into(),
                            )]),
                        ),
                    ]),
                );
            } else {
                audio.insert(
                    "controlPort".into(),
                    audio_control.local_addr()?.port().into(),
                );
            }
            let mut audio_request = if first.is_none() { root } else { dict(vec![]) };
            let audio_root = audio_request.as_dictionary_mut().unwrap();
            audio_root.remove("updateSessionRequest");
            audio_root.remove("combinedGetInfoWithControlSetup");
            audio_root.insert("streams".into(), vec![Value::Dictionary(audio)].into());
            let (response, audio_headers) = setup_full(
                &mut control,
                &audio_uri,
                &config.session_uuid,
                audio_request,
            )
            .await
            .context("audio SETUP")?;
            accepted = true;
            if config.timing == TimingMode::Ntp
                && !timing_probes
                && let Some(port) = response
                    .as_dictionary()
                    .and_then(|response| response.get("timingPort"))
                    .and_then(Value::as_unsigned_integer)
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port != 0)
            {
                timing::send_probes(&timing_socket, endpoint(peer.ip(), port)).await?;
            }
            let ((session_response, headers), received_at) = first
                .unwrap_or_else(|| ((response.clone(), audio_headers), std::time::Instant::now()));
            let session = session_response
                .as_dictionary()
                .context("session SETUP is not a dictionary")?;
            let skip_record = session
                .get("skipRecord")
                .and_then(Value::as_boolean)
                .unwrap_or(false);
            let clock = if config.timing == TimingMode::Ptp {
                let id = session
                    .get("timingPeerInfo")
                    .and_then(Value::as_dictionary)
                    .and_then(|peer| peer.get("ClockID"))
                    .and_then(Value::as_unsigned_integer)
                    .ok_or(MissingReceiverTimeline)?;
                let timestamp = match headers
                    .get("x-apple-requestreceivedtimestamp")
                    .and_then(|v| v.parse::<u64>().ok())
                    .zip(
                        headers
                            .get("x-apple-processingtime")
                            .and_then(|v| v.parse::<u64>().ok()),
                    )
                    .and_then(|(a, b)| a.checked_add(b))
                {
                    Some(ms) => timing::compact_time(Duration::from_millis(ms)),
                    None => timing::compact_time(timing::boot_time()?),
                };
                Some(timing::MediaClock::new(received_at, timestamp, id)?)
            } else {
                None
            };
            if let Some(port) = session
                .get("eventPort")
                .and_then(Value::as_unsigned_integer)
            {
                let port = u16::try_from(port)?;
                ensure!(port != 0, "invalid event port");
                let cancel_event = cancel.clone();
                let channel = events.clone();
                let secret = shared;
                tasks.push(tokio::spawn(async move {
                    let _ = event::serve(endpoint(peer.ip(), port), &secret, channel, cancel_event)
                        .await;
                }));
            }
            let data_port = port(&response, 96, "dataPort")?;
            let control_port = port(&response, 96, "controlPort")?;
            audio_data.connect(endpoint(peer.ip(), data_port)).await?;
            audio_control
                .connect(endpoint(peer.ip(), control_port))
                .await?;
            let timing_info = ["SubSu", "BePxT", "AfPxT", "BefEn", "EmEnc"]
                .into_iter()
                .map(|name| dict(vec![("name", name.into())]))
                .collect::<Vec<_>>();
            let video = dict(vec![
                ("type", 110u64.into()),
                ("streamConnectionID", video_id.into()),
                ("latencyMs", config.video_latency_ms.into()),
                ("timestampInfo", timing_info.into()),
                ("shk", Value::Data(video_master.to_vec())),
                ("shiv", Value::Data(video_iv.to_vec())),
            ]);
            let response = setup(
                &mut control,
                &video_uri,
                &config.session_uuid,
                dict(vec![("streams", vec![video].into())]),
            )
            .await
            .context("video SETUP")?;
            let video_port = port(&response, 110, "dataPort").context("video SETUP ports")?;
            let stream = tokio::time::timeout(
                Duration::from_secs(5),
                TcpStream::connect(endpoint(peer.ip(), video_port)),
            )
            .await
            .context("video TCP connect timed out")??;
            stream.set_nodelay(true)?;
            if !skip_record {
                let response = control
                    .request(
                        "RECORD",
                        &audio_uri,
                        &[
                            ("Session", &config.session_uuid),
                            ("Range", "npt=0-"),
                            ("RTP-Info", "seq=0;rtptime=0"),
                        ],
                        &[],
                    )
                    .await
                    .context("RECORD")?;
                ensure!(
                    (200..300).contains(&response.status),
                    "RECORD rejected: {}",
                    response.status
                );
            }
            let video_key = derive(
                &shared,
                format!("DataStream-Salt{video_id}").as_bytes(),
                b"DataStream-Output-Encryption-Key",
            );
            Ok((
                VideoSender::new(stream, video_key),
                AudioSender::new(
                    audio_data,
                    audio_control,
                    audio_key,
                    OsRng.next_u32(),
                    0,
                    config.audio_aad,
                ),
                clock,
            ))
        }
        .await;
        match result {
            Ok((video, audio, clock)) => {
                guard.disarm();
                Ok(Self {
                    control,
                    uri: audio_uri,
                    session_uuid: config.session_uuid,
                    video: Some(video),
                    audio,
                    cancel,
                    tasks,
                    clock,
                    video_latency_ms: config.video_latency_ms,
                    audio_latency_samples: config.audio_latency_samples,
                })
            }
            Err(error) => {
                if accepted {
                    let _ = teardown(&mut control, &audio_uri, &config.session_uuid).await;
                }
                cancel.cancel();
                for task in tasks {
                    task.abort();
                }
                Err(error)
            }
        }
    }
    pub async fn stop(mut self) -> Result<()> {
        let result = teardown(&mut self.control, &self.uri, &self.session_uuid).await;
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
        result
    }
    pub async fn feedback(&mut self) -> Result<()> {
        let response = self
            .control
            .request("POST", "/feedback", &[("Session", &self.session_uuid)], &[])
            .await?;
        ensure!(
            (200..300).contains(&response.status),
            "feedback rejected: {}",
            response.status
        );
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::hap::RecordCipher;
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn read_message(
        stream: &mut TcpStream,
        cipher: &mut RecordCipher,
        pending: &mut Vec<u8>,
    ) -> (String, Vec<u8>) {
        loop {
            if let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&pending[..end]).unwrap();
                let len: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if pending.len() >= end + 4 + len {
                    let method = headers
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .to_owned();
                    let body = pending[end + 4..end + 4 + len].to_vec();
                    pending.drain(..end + 4 + len);
                    return (method, body);
                }
            }
            let mut header = [0; 2];
            stream.read_exact(&mut header).await.unwrap();
            let len = u16::from_le_bytes(header) as usize;
            let mut record = vec![0; len + 18];
            record[..2].copy_from_slice(&header);
            stream.read_exact(&mut record[2..]).await.unwrap();
            pending.extend_from_slice(&cipher.open(&record).unwrap());
        }
    }
    async fn reply(stream: &mut TcpStream, cipher: &mut RecordCipher, sequence: u32, body: &[u8]) {
        let mut plain = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {sequence}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        plain.extend_from_slice(body);
        for chunk in plain.chunks(1024) {
            stream
                .write_all(&cipher.seal(chunk).unwrap())
                .await
                .unwrap();
        }
    }
    fn plist(value: Value) -> Vec<u8> {
        let mut out = Vec::new();
        value.to_writer_binary(&mut out).unwrap();
        out
    }
    #[tokio::test]
    async fn setup_record_video_and_teardown() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = listener.local_addr().unwrap();
        let video = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let video_port = video.local_addr().unwrap().port();
        let audio = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio_port = audio.local_addr().unwrap().port();
        let audio_control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let control_port = audio_control.local_addr().unwrap().port();
        let write_key = [1; 32];
        let read_key = [2; 32];
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut read = RecordCipher::new(write_key);
            let mut write = RecordCipher::new(read_key);
            let mut pending = Vec::new();
            let (method, body) = read_message(&mut stream, &mut read, &mut pending).await;
            assert_eq!(method, "SETUP");
            let root = Value::from_reader(Cursor::new(body)).unwrap();
            assert_eq!(
                root.as_dictionary().unwrap()["timingProtocol"].as_string(),
                Some("NTP")
            );
            reply(&mut stream, &mut write, 1, &plist(dict(vec![]))).await;
            let (method, body) = read_message(&mut stream, &mut read, &mut pending).await;
            assert_eq!(method, "SETUP");
            let root = Value::from_reader(Cursor::new(body)).unwrap();
            let streams = root.as_dictionary().unwrap()["streams"].as_array().unwrap();
            assert_eq!(
                streams[0].as_dictionary().unwrap()["type"].as_unsigned_integer(),
                Some(96)
            );
            let audio_key: [u8; 32] = streams[0].as_dictionary().unwrap()["shk"]
                .as_data()
                .unwrap()
                .try_into()
                .unwrap();
            let connection = dict(vec![
                (
                    "streamConnectionTypeRTP",
                    dict(vec![("streamConnectionKeyPort", audio_port.into())]),
                ),
                (
                    "streamConnectionTypeRTCP",
                    dict(vec![("streamConnectionKeyPort", control_port.into())]),
                ),
            ]);
            reply(
                &mut stream,
                &mut write,
                2,
                &plist(dict(vec![(
                    "streams",
                    vec![dict(vec![
                        ("type", 96u64.into()),
                        ("streamConnections", connection),
                    ])]
                    .into(),
                )])),
            )
            .await;
            let (method, body) = read_message(&mut stream, &mut read, &mut pending).await;
            assert_eq!(method, "SETUP");
            let video_setup = Value::from_reader(Cursor::new(body)).unwrap();
            let video_id = video_setup.as_dictionary().unwrap()["streams"]
                .as_array()
                .unwrap()[0]
                .as_dictionary()
                .unwrap()["streamConnectionID"]
                .as_unsigned_integer()
                .unwrap();
            reply(
                &mut stream,
                &mut write,
                3,
                &plist(dict(vec![(
                    "streams",
                    vec![dict(vec![
                        ("type", 110u64.into()),
                        ("dataPort", video_port.into()),
                    ])]
                    .into(),
                )])),
            )
            .await;
            let (mut media, _) = video.accept().await.unwrap();
            let (method, _) = read_message(&mut stream, &mut read, &mut pending).await;
            assert_eq!(method, "RECORD");
            reply(&mut stream, &mut write, 4, &[]).await;
            let mut header = [0; 128];
            media.read_exact(&mut header).await.unwrap();
            assert_eq!(header[4], 1);
            let codec_len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let mut payload = vec![0; codec_len];
            media.read_exact(&mut payload).await.unwrap();
            media.read_exact(&mut header).await.unwrap();
            let size = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let mut encrypted_video = vec![0; size];
            media.read_exact(&mut encrypted_video).await.unwrap();
            use chacha20poly1305::{
                ChaCha20Poly1305, KeyInit, Nonce,
                aead::{Aead, Payload},
            };
            let video_key = derive(
                &[3; 32],
                format!("DataStream-Salt{video_id}").as_bytes(),
                b"DataStream-Output-Encryption-Key",
            );
            let plaintext = ChaCha20Poly1305::new((&video_key).into())
                .decrypt(
                    Nonce::from_slice(&[0; 12]),
                    Payload {
                        msg: &encrypted_video,
                        aad: &header,
                    },
                )
                .unwrap();
            use gstreamer::{self as gst, prelude::*};
            gst::init().unwrap();
            let decoder = gst::parse::launch("appsrc name=input format=time ! h264parse ! avdec_h264 ! appsink name=output sync=false")
                .unwrap().downcast::<gst::Pipeline>().unwrap();
            let input = decoder
                .by_name("input")
                .unwrap()
                .downcast::<gstreamer_app::AppSrc>()
                .unwrap();
            let output = decoder
                .by_name("output")
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap();
            input.set_caps(Some(
                &gst::Caps::builder("video/x-h264")
                    .field("stream-format", "avc")
                    .field("alignment", "au")
                    .field("codec_data", gst::Buffer::from_slice(payload))
                    .build(),
            ));
            decoder.set_state(gst::State::Playing).unwrap();
            input
                .push_buffer(gst::Buffer::from_slice(plaintext))
                .unwrap();
            input.end_of_stream().unwrap();
            let decoded = output
                .try_pull_sample(gst::ClockTime::from_seconds(5))
                .expect("transport video did not decode");
            let info = decoded.caps().unwrap().structure(0).unwrap();
            assert_eq!(info.get::<i32>("width").unwrap(), 320);
            assert_eq!(info.get::<i32>("height").unwrap(), 240);
            decoder.set_state(gst::State::Null).unwrap();
            let mut rtp = [0; 2048];
            let (size, _) = audio.recv_from(&mut rtp).await.unwrap();
            assert_eq!(size, 12 + 1416 + 16 + 8);
            assert_eq!(&rtp[..2], &[0x80, 0xe0]);
            let decoded = ChaCha20Poly1305::new((&audio_key).into())
                .decrypt(Nonce::from_slice(&[0; 12]), &rtp[12..size - 8])
                .unwrap();
            assert_eq!(
                decoded,
                super::super::codec::alac_stereo(&[0; 1408]).unwrap()
            );
            let (method, _) = read_message(&mut stream, &mut read, &mut pending).await;
            assert_eq!(method, "TEARDOWN");
            reply(&mut stream, &mut write, 5, &[]).await;
        });
        let mut client = Client::connect(peer, Duration::from_secs(2)).await.unwrap();
        client.enable_encryption(write_key, read_key).unwrap();
        let (events, _rx) = mpsc::channel(8);
        let mut ekey = vec![0; 72];
        ekey[..4].copy_from_slice(b"FPLY");
        let config = Config {
            peer,
            session_uuid: "test-session".into(),
            device_id: "02:00:00:00:00:01".into(),
            name: "test".into(),
            audio_latency_samples: 11025,
            video_latency_ms: 250,
            stream_connections: true,
            audio_aad: AudioAad::None,
            receiver_features: 0,
            timing: TimingMode::Ntp,
        };
        let mut session = Session::start(
            client,
            config,
            [3; 32],
            Some(FairPlayMaterial {
                ekey,
                iv: [4; 16],
                master_key: [5; 16],
            }),
            events,
        )
        .await
        .unwrap();
        let capture =
            super::super::capture::Capture::video(crate::media::Source::Test, 320, 240, 10, 500)
                .unwrap();
        let sample = loop {
            if let Some(sample) = capture.pull().unwrap() {
                break sample;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(sample.keyframe);
        let video = session.video.as_mut().unwrap();
        video
            .codec(sample.codec_data.as_ref().unwrap(), 320, 240, 0)
            .await
            .unwrap();
        video.frame(&sample.data, true, 0, 0).await.unwrap();
        session.send_pcm(&[0; 1408], 0).await.unwrap();
        session.stop().await.unwrap();
        task.await.unwrap();
    }
}

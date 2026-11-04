//! Protocol lifecycle for PIN/HAP receivers. Unsupported modes fail closed.
use super::{
    discovery::{self, Device},
    event,
    pairing::{Credentials, Pairer},
    rtsp::Client,
    session::{self, Session, TimingMode},
    transport::AudioAad,
};
use anyhow::{Context, Result, bail, ensure};
use plist::Value;
use std::{io::Cursor, net::SocketAddr, time::Duration};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Connected,
    PinRequested,
    Paired,
    Failed,
}

pub struct Manager {
    peer: SocketAddr,
    client: Option<Client>,
    pairer: Pairer,
    shared: Option<[u8; 32]>,
    pub features: u64,
    pub status_flags: u64,
    pub source_version: String,
    pub state: State,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires external DoubleTake test receiver binary"]
    async fn doubletake_lg_manager_pin_and_session() {
        use tokio::{
            io::{AsyncBufReadExt, BufReader},
            process::Command,
        };
        let path = std::env::var("AIRPLAY_TEST_RECEIVER").expect("set AIRPLAY_TEST_RECEIVER");
        let mut child = Command::new(path)
            .args([
                "-listen",
                "127.0.0.1:0",
                "-profile",
                "lg",
                "-auth",
                "pin",
                "-code",
                "1234",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let peer = line.split_whitespace().nth(5).unwrap().parse().unwrap();
        let mut manager = Manager::connect(
            peer,
            "12345678-1234-4234-8234-123456789abc".into(),
            "Test Sender".into(),
        )
        .await
        .unwrap();
        assert_eq!(manager.features, 0x038bcb46007f8ad0);
        assert_ne!(manager.status_flags & (1 << 9), 0);
        manager.request_pin().await.unwrap();
        let _credentials = manager.submit_pin("1234").await.unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let session = manager
            .start(
                "8bf3a005-01da-4940-af31-d290ea098111".into(),
                "02:00:00:00:00:01".into(),
                tx,
            )
            .await
            .unwrap();
        session.stop().await.unwrap();
        child.kill().await.unwrap();
    }
}

impl Manager {
    pub async fn discover(window: Duration) -> Result<Vec<Device>> {
        discovery::discover(window).await
    }
    /// Connect and inspect receiver capabilities. No pairing or media is started.
    pub async fn connect(peer: SocketAddr, identifier: String, name: String) -> Result<Self> {
        let mut client = Client::connect(peer, Duration::from_secs(10)).await?;
        let info = client.request("GET", "/info", &[], &[]).await?;
        ensure!(
            (200..300).contains(&info.status),
            "receiver /info rejected: {}",
            info.status
        );
        let plist = Value::from_reader(Cursor::new(info.body)).context("invalid receiver /info")?;
        let fields = plist
            .as_dictionary()
            .context("receiver /info is not a dictionary")?;
        let features = fields
            .get("features")
            .and_then(Value::as_unsigned_integer)
            .context("receiver omitted features")?;
        let status_flags = fields
            .get("statusFlags")
            .and_then(Value::as_unsigned_integer)
            .unwrap_or(0);
        let source_version = fields
            .get("sourceVersion")
            .and_then(Value::as_string)
            .unwrap_or("")
            .to_owned();
        let modern = features & ((1 << 38) | (1 << 43) | (1 << 46) | (1 << 48)) != 0
            && features & ((1 << 26) | (1 << 51)) == 0;
        Ok(Self {
            peer,
            client: Some(client),
            pairer: Pairer {
                identifier,
                client_name: name,
                pairing_type: if modern { 5 } else { 3 },
            },
            shared: None,
            features,
            status_flags,
            source_version,
            state: State::Connected,
        })
    }
    pub async fn request_pin(&mut self) -> Result<()> {
        ensure!(
            self.state == State::Connected,
            "PIN request is invalid in this state"
        );
        self.pairer
            .start_pin(
                self.client
                    .as_mut()
                    .context("control connection is closed")?,
            )
            .await?;
        self.state = State::PinRequested;
        Ok(())
    }
    pub async fn submit_pin(&mut self, pin: &str) -> Result<Credentials> {
        ensure!(self.state == State::PinRequested, "PIN was not requested");
        let client = self
            .client
            .as_mut()
            .context("control connection is closed")?;
        let result = async {
            let credentials = self.pairer.pair_pin(client, pin).await?;
            let secret = self.pairer.verify(client, &credentials).await?;
            Ok::<_, anyhow::Error>((credentials, secret))
        }
        .await;
        let (credentials, secret) = match result {
            Ok(value) => value,
            Err(error) => {
                self.client.take();
                self.state = State::Failed;
                return Err(error);
            }
        };
        self.shared = Some(secret);
        self.state = State::Paired;
        Ok(credentials)
    }
    pub async fn resume(&mut self, credentials: &Credentials) -> Result<()> {
        ensure!(
            self.state == State::Connected,
            "resume is invalid in this state"
        );
        ensure!(
            self.pairer.identifier == credentials.identifier,
            "credential identity mismatch"
        );
        let result = self
            .pairer
            .verify(
                self.client
                    .as_mut()
                    .context("control connection is closed")?,
                credentials,
            )
            .await;
        let secret = match result {
            Ok(value) => value,
            Err(error) => {
                self.client.take();
                self.state = State::Failed;
                return Err(error);
            }
        };
        self.shared = Some(secret);
        self.state = State::Paired;
        Ok(())
    }
    /// Create the transport session. FPSAP receivers are rejected until native
    /// FairPlay is available, never downgraded to unencrypted media.
    pub async fn start(
        self,
        session_uuid: String,
        device_id: String,
        events: mpsc::Sender<event::Event>,
    ) -> Result<Session> {
        self.start_with_timing(session_uuid, device_id, events, None)
            .await
    }

    /// Start a session, optionally overriding the timing protocol. The override
    /// is used for the single NTP retry after a PTP receiver omits its timeline.
    pub async fn start_with_timing(
        mut self,
        session_uuid: String,
        device_id: String,
        events: mpsc::Sender<event::Event>,
        timing_override: Option<TimingMode>,
    ) -> Result<Session> {
        ensure!(self.state == State::Paired, "receiver is not paired");
        if self.features & (1 << 14) != 0 {
            bail!("receiver requires FairPlay SAP; native sender does not yet implement it");
        }
        let version: Vec<_> = self
            .source_version
            .split('.')
            .filter_map(|p| p.parse::<u64>().ok())
            .collect();
        // Samsung Tizen receivers report sourceVersion 377.x, advertise feature
        // 41 and PTPInfo, but their SETUP names no PTP timeline. DoubleTake only
        // discovered this after a failed PTP attempt and a reconnect; prefer NTP
        // up front for that family so the receiver is not left half-configured.
        let ptp_version = version.len() >= 2
            && (version[0], version[1], *version.get(2).unwrap_or(&0)) >= (354, 54, 6)
            && version[0] != 377;
        let timing = timing_override.unwrap_or_else(|| {
            if self.features & (1 << 41) != 0 && ptp_version {
                TimingMode::Ptp
            } else {
                TimingMode::Ntp
            }
        });
        let config = session::Config {
            peer: self.peer,
            session_uuid,
            device_id,
            name: self.pairer.client_name.clone(),
            // DoubleTake's high-latency (wireless) screen profile: 170 ms audio,
            // 100 ms video. The previous 250/250 added unnecessary delay.
            audio_latency_samples: 7497,
            video_latency_ms: 100,
            stream_connections: self.features & (1 << 59) != 0,
            audio_aad: AudioAad::TimestampSsrc,
            receiver_features: self.features,
            timing,
        };
        Session::start(
            self.client.take().context("control connection is closed")?,
            config,
            self.shared.take().context("missing pair-verify secret")?,
            None,
            events,
        )
        .await
    }
}

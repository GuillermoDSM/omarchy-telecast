use anyhow::{Result, ensure};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

/// Convert a duration to AirPlay's unsigned seconds.32 fraction format.
pub fn compact_time(duration: Duration) -> u64 {
    (duration.as_secs() << 32) | (((duration.subsec_nanos() as u64) << 32) / 1_000_000_000)
}

/// Receiver timeline anchored to a local monotonic instant.
pub struct MediaClock {
    anchor: Instant,
    timestamp: u64,
    pub timeline_id: u64,
}
impl MediaClock {
    pub fn update_timeline(&mut self, instant: Instant, timeline_id: u64) -> Result<()> {
        ensure!(timeline_id != 0, "invalid PTP timeline ID");
        self.timestamp = self
            .at(instant)
            .ok_or_else(|| anyhow::anyhow!("PTP time out of range"))?;
        self.anchor = instant;
        self.timeline_id = timeline_id;
        Ok(())
    }
    pub fn new(anchor: Instant, timestamp: u64, timeline_id: u64) -> Result<Self> {
        ensure!(timeline_id != 0, "receiver must supply PTP timeline ID");
        Ok(Self {
            anchor,
            timestamp,
            timeline_id,
        })
    }
    pub fn at(&self, at: Instant) -> Option<u64> {
        if at >= self.anchor {
            self.timestamp
                .checked_add(compact_time(at.duration_since(self.anchor)))
        } else {
            self.timestamp
                .checked_sub(compact_time(self.anchor.duration_since(at)))
        }
    }
    pub fn reanchor(&mut self, instant: Instant, timestamp: u64) {
        self.timestamp = self.at(instant).unwrap_or(timestamp).max(timestamp);
        self.anchor = instant;
    }
}

/// CLOCK_BOOTTIME matches AirPlay's boot-relative timestamps, including suspend.
pub fn boot_time() -> Result<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is valid writable storage for clock_gettime.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(
        ts.tv_sec >= 0 && (0..1_000_000_000).contains(&ts.tv_nsec),
        "invalid boot clock"
    );
    Ok(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

pub fn timing_reply(request: &[u8], now: u64) -> Option<[u8; 32]> {
    if request.len() < 32 || request[..2] != [0x80, 0xd2] {
        return None;
    }
    let mut reply = [0; 32];
    reply.copy_from_slice(&request[..32]);
    reply[1] = 0xd3;
    reply[8..16].copy_from_slice(&request[24..32]);
    reply[16..24].copy_from_slice(&now.to_be_bytes());
    reply[24..32].copy_from_slice(&now.to_be_bytes());
    Some(reply)
}

/// Initiate timing with receivers that expect sender probes on the same socket
/// used for their requests. Probes and replies are distinguished by 0xd2/0xd3.
pub async fn send_probes(socket: &UdpSocket, receiver: SocketAddr) -> Result<()> {
    for sequence in 1u16..=3 {
        let mut request = [0u8; 32];
        request[0] = 0x80;
        request[1] = 0xd2;
        request[2..4].copy_from_slice(&sequence.to_be_bytes());
        let now = compact_time(boot_time()?).wrapping_add(2_208_988_800u64 << 32);
        request[24..32].copy_from_slice(&now.to_be_bytes());
        socket.send_to(&request, receiver).await?;
        if sequence < 3 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(())
}

/// Respond only to the selected receiver; caller owns the UDP socket lifetime.
pub async fn serve_timing(
    socket: &UdpSocket,
    receiver: SocketAddr,
    cancel: CancellationToken,
) -> Result<()> {
    let mut buf = [0; 128];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            packet = socket.recv_from(&mut buf) => {
                let (n, peer) = packet?;
                if peer.ip() != receiver.ip() { continue; }
                let now = compact_time(boot_time()?).wrapping_add(2_208_988_800u64 << 32);
                if let Some(reply) = timing_reply(&buf[..n], now) { socket.send_to(&reply, peer).await?; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timing_packet() {
        let mut req = [0; 32];
        req[..2].copy_from_slice(&[0x80, 0xd2]);
        req[24..].copy_from_slice(&42u64.to_be_bytes());
        let reply = timing_reply(&req, 55).unwrap();
        assert_eq!(&reply[8..16], &42u64.to_be_bytes());
        assert_eq!(&reply[24..], &55u64.to_be_bytes());
        assert!(timing_reply(&reply, 0).is_none());
    }
    #[test]
    fn compact() {
        assert_eq!(compact_time(Duration::from_millis(1500)), 0x180000000);
    }
    #[tokio::test]
    async fn timing_server_replies_to_receiver() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async move {
            let _ = serve_timing(&server, address, cancel).await;
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut request = [0u8; 32];
        request[0] = 0x80;
        request[1] = 0xd2;
        request[24..].copy_from_slice(&42u64.to_be_bytes());
        client.send_to(&request, address).await.unwrap();
        let mut reply = [0u8; 32];
        let (length, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(length, 32);
        assert_eq!(&reply[..2], &[0x80, 0xd3]);
        assert_eq!(&reply[8..16], &42u64.to_be_bytes());
        assert_ne!(&reply[24..], &[0u8; 8]);
    }
}

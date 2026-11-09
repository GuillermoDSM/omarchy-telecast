use anyhow::{Result, ensure};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use std::collections::VecDeque;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpStream, UdpSocket},
};

/// A stream key must come from a successfully authenticated AirPlay handshake.
pub struct VideoSender {
    stream: TcpStream,
    cipher: ChaCha20Poly1305,
    nonce: u64,
}
impl VideoSender {
    pub fn new(stream: TcpStream, key: [u8; 32]) -> Self {
        Self {
            stream,
            cipher: ChaCha20Poly1305::new((&key).into()),
            nonce: 0,
        }
    }
    pub async fn codec(
        &mut self,
        avcc: &[u8],
        width: u32,
        height: u32,
        timestamp: u64,
    ) -> Result<()> {
        let size = u32::try_from(avcc.len())?;
        let mut header = [0; 128];
        header[..4].copy_from_slice(&size.to_le_bytes());
        header[4] = 1;
        header[6] = 0x16;
        header[7] = 1;
        header[8..16].copy_from_slice(&timestamp.to_le_bytes());
        for (offset, value) in [
            (16, width),
            (20, height),
            (40, width),
            (44, height),
            (56, width),
            (60, height),
        ] {
            header[offset..offset + 4].copy_from_slice(&(value as f32).to_le_bytes());
        }
        self.stream.write_all(&header).await?;
        self.stream.write_all(avcc).await?;
        Ok(())
    }
    /// `avcc` is a complete access unit with big-endian 4-byte NAL lengths.
    pub async fn frame(
        &mut self,
        avcc: &[u8],
        keyframe: bool,
        timestamp: u64,
        timeline_id: u64,
    ) -> Result<()> {
        validate_avcc(avcc)?;
        let size = u32::try_from(
            avcc.len()
                .checked_add(16)
                .ok_or_else(|| anyhow::anyhow!("frame too large"))?,
        )?;
        let mut header = [0; 128];
        header[..4].copy_from_slice(&size.to_le_bytes());
        header[5] = if keyframe { 0x10 } else { 0 };
        header[8..16].copy_from_slice(&timestamp.to_le_bytes());
        header[40..48].copy_from_slice(&timeline_id.to_le_bytes());
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&self.nonce.to_le_bytes());
        // Reserve the nonce before any I/O, including on failed writes.
        self.nonce = self
            .nonce
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("video nonce exhausted"))?;
        let encrypted = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: avcc,
                    aad: &header,
                },
            )
            .map_err(|_| anyhow::anyhow!("video encryption failed"))?;
        self.stream.write_all(&header).await?;
        self.stream.write_all(&encrypted).await?;
        Ok(())
    }
}

fn validate_avcc(mut data: &[u8]) -> Result<()> {
    ensure!(!data.is_empty(), "empty video access unit");
    while !data.is_empty() {
        ensure!(data.len() >= 4, "truncated NAL length");
        let n = u32::from_be_bytes(data[..4].try_into()?) as usize;
        ensure!(n > 0 && n <= data.len() - 4, "invalid NAL length");
        data = &data[4 + n..];
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum AudioAad {
    /// Used by protocol tests; the manager selects TimestampSsrc.
    #[allow(dead_code)]
    None,
    TimestampSsrc,
}

/// Connected UDP sockets ensure retransmissions cannot be requested by other hosts.
pub struct AudioSender {
    data: UdpSocket,
    control: UdpSocket,
    cipher: ChaCha20Poly1305,
    aad: AudioAad,
    nonce: u64,
    sequence: u16,
    ssrc: u32,
    marker: bool,
    history: VecDeque<(u16, Vec<u8>)>,
}
impl AudioSender {
    /// Receive and answer one receiver retransmission request from the connected control socket.
    pub async fn receive_retransmit(&self) -> Result<usize> {
        let mut request = [0u8; 128];
        let size = self.control.recv(&mut request).await?;
        self.retransmit(&request[..size]).await
    }
    pub fn new(
        data: UdpSocket,
        control: UdpSocket,
        key: [u8; 32],
        ssrc: u32,
        sequence: u16,
        aad: AudioAad,
    ) -> Self {
        Self {
            data,
            control,
            cipher: ChaCha20Poly1305::new((&key).into()),
            aad,
            nonce: 0,
            sequence,
            ssrc,
            marker: true,
            history: VecDeque::new(),
        }
    }
    pub async fn send(&mut self, payload: &[u8], timestamp: u32, payload_type: u8) -> Result<()> {
        ensure!(payload_type < 128, "invalid RTP payload type");
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[1] = payload_type | if self.marker { 0x80 } else { 0 };
        header[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        header[4..8].copy_from_slice(&timestamp.to_be_bytes());
        header[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        let aad = match self.aad {
            AudioAad::None => &[][..],
            AudioAad::TimestampSsrc => &header[4..12],
        };
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.nonce.to_le_bytes());
        let encrypted = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: payload, aad })
            .map_err(|_| anyhow::anyhow!("audio encryption failed"))?;
        let mut packet = header.to_vec();
        packet.extend_from_slice(&encrypted);
        packet.extend_from_slice(&self.nonce.to_le_bytes());
        self.nonce = self
            .nonce
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("audio nonce exhausted"))?;
        let seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        self.marker = false;
        // Replay the two preceding encrypted datagrams byte-for-byte, preserving
        // their original sequence and nonce as required by redundantAudio=2.
        for (_, previous) in self
            .history
            .iter()
            .rev()
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            self.data.send(previous).await?;
        }
        self.data.send(&packet).await?;
        self.history.push_back((seq, packet));
        if self.history.len() > 512 {
            self.history.pop_front();
        }
        Ok(())
    }
    /// Respond to one already-received retransmission request; bounded amplification.
    pub async fn retransmit(&self, request: &[u8]) -> Result<usize> {
        ensure!(
            request.len() == 8 && request[..2] == [0x80, 0xd5],
            "invalid retransmit request"
        );
        let id = &request[2..4];
        let first = u16::from_be_bytes(request[4..6].try_into()?);
        let count = u16::from_be_bytes(request[6..8].try_into()?) as usize;
        ensure!(count > 0, "empty retransmit request");
        let mut sent = 0;
        for offset in 0..count.min(self.history.len() + 1) {
            let seq = first.wrapping_add(offset as u16);
            let mut response = vec![0x80, 0xd6, id[0], id[1]];
            if let Some((_, packet)) = self.history.iter().find(|(n, _)| *n == seq) {
                response.extend_from_slice(packet);
            } else {
                response.extend_from_slice(&seq.to_be_bytes());
                response.extend_from_slice(&[0, 0]);
                self.control.send(&response).await?;
                break;
            }
            self.control.send(&response).await?;
            sent += 1;
        }
        Ok(sent)
    }
    pub async fn sync(
        &self,
        network_time: u64,
        timeline_id: Option<u64>,
        rtp: u32,
        latency: u32,
        first: bool,
    ) -> Result<()> {
        let mut packet = vec![0u8; if timeline_id.is_some() { 28 } else { 20 }];
        packet[0] = if first { 0x90 } else { 0x80 };
        packet[1] = if timeline_id.is_some() { 0xd7 } else { 0xd4 };
        packet[2..4].copy_from_slice(&4u16.to_be_bytes());
        let playhead = rtp.wrapping_sub(latency);
        packet[4..8].copy_from_slice(&playhead.to_be_bytes());
        let time = if timeline_id.is_some() {
            (network_time >> 32) * 1_000_000_000
                + (((network_time & 0xffff_ffff) * 1_000_000_000) >> 32)
        } else {
            network_time
        };
        packet[8..16].copy_from_slice(&time.to_be_bytes());
        packet[16..20].copy_from_slice(&playhead.to_be_bytes());
        if let Some(id) = timeline_id {
            ensure!(id != 0, "invalid PTP timeline");
            packet[20..28].copy_from_slice(&id.to_be_bytes());
        }
        self.control.send(&packet).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn avcc_validation() {
        assert!(validate_avcc(&[0, 0, 0, 2, 1, 2]).is_ok());
        assert!(validate_avcc(&[0, 0, 0, 3, 1, 2]).is_err());
        assert!(validate_avcc(&[0, 0, 0, 0]).is_err());
    }
}

//! RFC 3550 compound packet parsing. Only reports for our own SSRC are used.
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct Report {
    pub loss: f64,
    pub jitter_ms: f64,
    pub highest_seq: u32,
}

pub fn reports(data: &[u8], our_ssrc: u32) -> Vec<Report> {
    let mut result = Vec::new();
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let p = &data[pos..];
        if p[0] >> 6 != 2 {
            break;
        }
        let length = (u16::from_be_bytes([p[2], p[3]]) as usize + 1) * 4;
        if length < 4 || length > p.len() {
            break;
        }
        let count = (p[0] & 31) as usize;
        let offset = match p[1] {
            201 => 8,
            200 => 28,
            _ => {
                pos += length;
                continue;
            }
        };
        if length < offset + count * 24 {
            break;
        }
        for i in 0..count {
            let block = &p[offset + i * 24..offset + (i + 1) * 24];
            if u32::from_be_bytes(block[0..4].try_into().unwrap()) == our_ssrc {
                result.push(Report {
                    loss: block[4] as f64 / 256.0,
                    jitter_ms: u32::from_be_bytes(block[12..16].try_into().unwrap()) as f64 / 90.0,
                    highest_seq: u32::from_be_bytes(block[8..12].try_into().unwrap()),
                });
            }
        }
        pos += length;
    }
    result
}

pub fn sender_report(ssrc: u32, timestamp: u32, packets: u32, bytes: u32) -> Vec<u8> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = (now.as_secs() + 2_208_988_800) as u32;
    let frac = ((now.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000) as u32;
    let mut out = vec![0x80, 200, 0, 6];
    for value in [ssrc, secs, frac, timestamp, packets, bytes] {
        out.extend(value.to_be_bytes());
    }
    // Every compound packet must include an SDES CNAME; use a non-identifying,
    // per-stream SSRC rather than the user's hostname or address.
    let cname = format!("omarchy-{ssrc:08x}");
    let mut sdes = vec![0x81, 202, 0, 0];
    sdes.extend(ssrc.to_be_bytes());
    sdes.extend([1, cname.len() as u8]);
    sdes.extend(cname.as_bytes());
    sdes.push(0);
    while sdes.len() % 4 != 0 {
        sdes.push(0);
    }
    let words = (sdes.len() / 4 - 1) as u16;
    sdes[2..4].copy_from_slice(&words.to_be_bytes());
    out.extend(sdes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_our_report_and_handles_truncation() {
        let mut packet = vec![0x81, 201, 0, 7];
        packet.extend(12u32.to_be_bytes());
        packet.extend(42u32.to_be_bytes());
        packet.extend([16, 0, 0, 0]);
        packet.extend(1u32.to_be_bytes());
        packet.extend(9000u32.to_be_bytes());
        packet.extend([0; 8]);
        let r = reports(&packet, 42);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].loss, 0.0625);
        assert_eq!(r[0].jitter_ms, 100.0);
        assert!(reports(&packet, 43).is_empty());
        for end in 0..packet.len() {
            assert!(reports(&packet[..end], 42).is_empty());
        }
    }
    #[test]
    fn sender_report_is_well_formed_compound_packet() {
        let packet = sender_report(42, 1000, 30, 4000);
        assert_eq!(&packet[..4], &[0x80, 200, 0, 6]);
        assert_eq!(packet[29], 202);
        assert_eq!(packet.len() % 4, 0);
        assert!(reports(&packet, 42).is_empty());
    }
}

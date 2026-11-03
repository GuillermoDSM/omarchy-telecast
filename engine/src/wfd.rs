//! Wi-Fi Display 1.0 parameter negotiation. No HDCP, UIBC or extensions.
use anyhow::{Context, Result, bail, ensure};

/// AAC-LC stereo at 48 kHz is WFD AAC mode bit 0.
pub fn supports_aac(value: &str) -> bool {
    value.split(',').any(|entry| {
        let mut fields = entry.split_whitespace();
        matches!((fields.next(), fields.next(), fields.next(), fields.next()),
            (Some("AAC"), Some(modes), Some(latency), None)
                if u32::from_str_radix(modes, 16).is_ok_and(|m| m & 1 != 0)
                    && u8::from_str_radix(latency, 16).is_ok())
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub cea_bit: u32,
    pub profile: u8,
    pub level: u8,
}

impl VideoMode {
    pub fn gst_profile(&self) -> &'static str {
        if self.profile == 2 {
            "high"
        } else {
            "constrained-baseline"
        }
    }

    pub fn gst_level(&self) -> &'static str {
        match self.level {
            1 => "3.1",
            2 => "3.2",
            4 => "4",
            8 => "4.1",
            _ => "4.2",
        }
    }

    pub fn parameter(&self) -> String {
        // Native byte: CEA index in bits 7:3, CEA table selector in bits 2:0.
        let native = (self.cea_bit.trailing_zeros() as u8) << 3;
        format!(
            "{native:02x} 00 {:02x} {:02x} {:08x} 00000000 00000000 00 0000 0000 00 none none",
            self.profile, self.level, self.cea_bit
        )
    }
}

/// Prefer 1080p30, then 720p30, finally the mandatory VGA60 fallback.
/// VGA60 is below the 1080p/30 bandwidth budget, not a 60-Hz high-res mode.
pub fn select_video(value: &str) -> Result<VideoMode> {
    let mut codecs = Vec::new();
    for (index, codec) in value.split(',').enumerate() {
        let fields: Vec<_> = codec.split_whitespace().collect();
        let offset = if index == 0 { 2 } else { 0 };
        ensure!(
            fields.len() >= offset + 11,
            "Formato WFD de vídeo incompleto"
        );
        let hex = |i| {
            u32::from_str_radix(fields[offset + i], 16)
                .context("Capacidad WFD hexadecimal inválida")
        };
        codecs.push((hex(0)?, hex(1)?, hex(2)?));
    }
    for (width, height, fps, bit, minimum_level) in [
        (1920, 1080, 30, 1 << 7, 4),
        (1280, 720, 30, 1 << 5, 1),
        (640, 480, 60, 1, 1),
    ] {
        for &(profiles, levels, cea) in &codecs {
            if cea & bit == 0 {
                continue;
            }
            let profile = if profiles & 2 != 0 {
                2
            } else if profiles & 1 != 0 {
                1
            } else {
                continue;
            };
            if let Some(level) = [1, 2, 4, 8, 16]
                .into_iter()
                .find(|l| *l >= minimum_level && levels & l != 0)
            {
                return Ok(VideoMode {
                    width,
                    height,
                    fps,
                    cea_bit: bit,
                    profile,
                    level: level as u8,
                });
            }
        }
    }
    bail!("La TV no anuncia un modo H.264 compatible de hasta 1080p")
}

pub fn parameter<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    body.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then_some(value.trim())
    })
}

pub fn rtp_port(value: &str) -> Result<u16> {
    let fields: Vec<_> = value.split_whitespace().collect();
    ensure!(
        fields.len() == 4 && fields[0] == "RTP/AVP/UDP;unicast" && fields[3] == "mode=play",
        "La TV no ofrece transporte RTP/UDP unicast compatible"
    );
    let port: u16 = fields[1].parse().context("Puerto RTP inválido")?;
    ensure!((1024..65535).contains(&port), "Puerto RTP fuera de rango");
    Ok(port)
}

pub fn client_ports(transport: &str) -> Result<(u16, Option<u16>)> {
    ensure!(
        transport.starts_with("RTP/AVP/UDP;") || transport.starts_with("RTP/AVP;"),
        "Solo se admite RTP/UDP"
    );
    ensure!(
        transport.split(';').any(|s| s == "unicast"),
        "Solo se admite unicast"
    );
    ensure!(
        !transport.contains("destination=") && !transport.contains("interleaved="),
        "Destino o transporte SETUP no permitido"
    );
    let ports = transport
        .split(';')
        .find_map(|s| s.strip_prefix("client_port="))
        .context("SETUP sin client_port")?;
    let (first, second) = ports
        .split_once('-')
        .map_or((ports, None), |(a, b)| (a, Some(b)));
    let rtp: u16 = first.parse()?;
    ensure!((1024..65535).contains(&rtp), "Puerto RTP fuera de rango");
    let rtcp = second.map(str::parse::<u16>).transpose()?;
    if let Some(port) = rtcp {
        ensure!(
            port == rtp + 1,
            "Los puertos RTP y RTCP no son consecutivos"
        );
    }
    Ok((rtp, rtcp))
}

/// WFD Device Information subelement: source, session available, TCP 7236,
/// 50 Mb/s maximum throughput. This is NOT an arbitrary UDP listen port.
pub fn source_ies() -> Vec<u8> {
    vec![0, 0, 6, 0, 0x10, 0x1c, 0x44, 0, 50]
}

pub fn available_sink(ies: &[u8]) -> bool {
    let mut rest = ies;
    while rest.len() >= 3 {
        let length = u16::from_be_bytes([rest[1], rest[2]]) as usize;
        if rest.len() < 3 + length {
            return false;
        }
        if rest[0] == 0 && length >= 6 {
            let info = u16::from_be_bytes([rest[3], rest[4]]);
            let role = info & 3;
            return matches!(role, 1 | 3) && info & 0x30 == 0x10;
        }
        rest = &rest[3 + length..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_valid_aac_stereo_capabilities() {
        assert!(supports_aac("LPCM 00000003 00, AAC 00000005 00"));
        assert!(!supports_aac("LPCM 00000003 00"));
        assert!(!supports_aac("AAC 00000002 00"));
        assert!(!supports_aac("AAC 00000001 invalid"));
        assert!(!supports_aac("AAC 00000001 00 extra"));
    }
    #[test]
    fn caps_never_select_4k_or_1080p60() {
        let mode = select_video("00 00 02 1f ffffffff ffffffff 00000000 00 0000 0000 00 none none")
            .unwrap();
        assert_eq!((mode.width, mode.height, mode.fps), (1920, 1080, 30));
        assert!(mode.parameter().contains("00000080"));
    }
    #[test]
    fn modes_profiles_and_levels_must_overlap() {
        let mode = select_video("00 00 01 01 000000a1 00000000 00000000 00 0000 0000 00 none none")
            .unwrap();
        assert_eq!((mode.width, mode.height, mode.profile), (1280, 720, 1));
        assert!(select_video("none").is_err());
        assert!(
            select_video("00 00 04 1f ffffffff 00000000 00000000 00 0000 0000 00 none none")
                .is_err()
        );
    }
    #[test]
    fn validates_ports_and_rejects_redirects() {
        assert_eq!(
            client_ports("RTP/AVP/UDP;unicast;client_port=19000-19001").unwrap(),
            (19000, Some(19001))
        );
        assert!(client_ports("RTP/AVP/TCP;unicast;client_port=19000").is_err());
        assert!(client_ports("RTP/AVP/UDP;unicast;client_port=19000;destination=8.8.8.8").is_err());
        assert!(client_ports("RTP/AVP/UDP;unicast;client_port=65535-0").is_err());
        assert!(client_ports("RTP/AVP/UDP;unicast;client_port=19000-19002").is_err());
    }
    #[test]
    fn discovery_filters_sources_busy_sinks_and_bad_ies() {
        let mut ies = source_ies();
        assert!(!available_sink(&ies));
        ies[4] = 0x11;
        assert!(available_sink(&ies));
        ies[4] = 0x01;
        assert!(!available_sink(&ies));
        assert!(!available_sink(&ies[..5]));
    }
}

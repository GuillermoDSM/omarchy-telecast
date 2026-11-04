//! Elementary media framing for the AirPlay screen/audio transports.
use anyhow::{Result, ensure};

/// Encode one 352-sample stereo S16LE PCM frame as a verbatim ALAC access unit.
/// This format deliberately trades bandwidth for avoiding a separate encoder.
pub fn alac_stereo(pcm: &[u8]) -> Result<Vec<u8>> {
    const SAMPLES: usize = 352;
    ensure!(
        pcm.len() == SAMPLES * 4,
        "ALAC frame requires 352 stereo S16LE samples"
    );
    struct Bits {
        bytes: Vec<u8>,
        used: u8,
    }
    impl Bits {
        fn put(&mut self, value: u32, count: u8) {
            for bit in (0..count).rev() {
                if self.used == 0 {
                    self.bytes.push(0);
                }
                *self.bytes.last_mut().unwrap() |= (((value >> bit) & 1) as u8) << (7 - self.used);
                self.used = (self.used + 1) % 8;
            }
        }
    }
    let mut bits = Bits {
        bytes: Vec::with_capacity(1416),
        used: 0,
    };
    bits.put(1, 3);
    bits.put(0, 4);
    bits.put(0, 12);
    bits.put(1, 1);
    bits.put(0, 2);
    bits.put(1, 1);
    bits.put(SAMPLES as u32, 32);
    for sample in pcm.chunks_exact(2) {
        bits.put(u16::from_le_bytes([sample[0], sample[1]]) as u32, 16);
    }
    bits.put(7, 3);
    Ok(bits.bytes)
}

/// Build an AVCDecoderConfigurationRecord from raw H.264 SPS and PPS NAL units.
pub fn avcc_configuration(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    ensure!(sps.len() >= 4 && sps[0] & 0x1f == 7, "invalid H.264 SPS");
    ensure!(!pps.is_empty() && pps[0] & 0x1f == 8, "invalid H.264 PPS");
    let sps_len = u16::try_from(sps.len())?;
    let pps_len = u16::try_from(pps.len())?;
    let mut out = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    out.extend_from_slice(&sps_len.to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&pps_len.to_be_bytes());
    out.extend_from_slice(pps);
    Ok(out)
}

/// Convert a list of raw H.264 NAL units into a length-prefixed access unit.
pub fn avcc_access_unit(nals: &[&[u8]]) -> Result<Vec<u8>> {
    ensure!(!nals.is_empty(), "empty H.264 access unit");
    let mut out = Vec::new();
    for nal in nals {
        ensure!(!nal.is_empty(), "empty H.264 NAL");
        out.extend_from_slice(&u32::try_from(nal.len())?.to_be_bytes());
        out.extend_from_slice(nal);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn avcc_packet_layout() {
        assert_eq!(
            avcc_access_unit(&[&[0x65, 1], &[0x41]]).unwrap(),
            [0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x41]
        );
        assert!(avcc_access_unit(&[&[]]).is_err());
    }
    #[test]
    fn avcc_config_layout() {
        assert_eq!(
            avcc_configuration(&[0x67, 0x64, 0, 0x1f], &[0x68, 1]).unwrap(),
            [
                1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x1f, 1, 0, 2, 0x68, 1
            ]
        );
    }
    #[test]
    fn alac_silence_frame() {
        let frame = alac_stereo(&[0; 1408]).unwrap();
        assert_eq!(frame.len(), 1416);
        assert_eq!(&frame[..7], &[0x20, 0, 0x12, 0, 0, 2, 0xc0]);
        assert_eq!(&frame[1414..], &[1, 0xc0]);
        assert!(alac_stereo(&[0; 4]).is_err());
    }

    #[test]
    fn alac_roundtrip_with_gstreamer_decoder() {
        use gstreamer::{self as gst, prelude::*};
        gst::init().unwrap();
        let pcm: Vec<u8> = (0..704)
            .flat_map(|i| ((i as i16 * 41) - 14000).to_le_bytes())
            .collect();
        let encoded = alac_stereo(&pcm).unwrap();
        let cookie = [
            0, 0, 0, 36, b'a', b'l', b'a', b'c', 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 16, 40, 10, 14, 2,
            0, 0, 0, 0, 0x40, 4, 0, 0x15, 0x88, 0x80, 0, 0, 0xac, 0x44,
        ];
        let pipeline = gst::parse::launch("appsrc name=input format=time ! avdec_alac ! audioconvert ! audio/x-raw,format=S16LE,layout=interleaved ! appsink name=output sync=false").unwrap().downcast::<gst::Pipeline>().unwrap();
        let input = pipeline
            .by_name("input")
            .unwrap()
            .downcast::<gstreamer_app::AppSrc>()
            .unwrap();
        let output = pipeline
            .by_name("output")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        let caps = gst::Caps::builder("audio/x-alac")
            .field("channels", 2i32)
            .field("rate", 44100i32)
            .field("codec_data", gst::Buffer::from_slice(cookie))
            .build();
        input.set_caps(Some(&caps));
        pipeline.set_state(gst::State::Playing).unwrap();
        input.push_buffer(gst::Buffer::from_slice(encoded)).unwrap();
        input.end_of_stream().unwrap();
        let sample = output
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .expect("ALAC decoder produced no frame");
        let buffer = sample.buffer().unwrap().map_readable().unwrap();
        assert_eq!(buffer.as_slice(), pcm);
        pipeline.set_state(gst::State::Null).unwrap();
    }
}

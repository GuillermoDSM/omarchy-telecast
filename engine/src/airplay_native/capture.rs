//! Elementary capture built on the existing portal-authorised `media::Source`.
//! Keep the `media::Capture` (and its restricted PipeWire FD) alive while this runs.
use crate::media::Source;
use anyhow::{Context, Result, ensure};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::AppSink;

/// 1080p30 screen/video needs more than DoubleTake's conservative 4.1 Mb/s.
/// The link is only a few Mb/s away from idle on Wi-Fi 6, so 8 Mb/s removes
/// fast-motion macroblocking without adding meaningful network latency.
pub const DEFAULT_BITRATE_KBPS: u32 = 8000;

pub struct Sample {
    pub data: Vec<u8>,
    pub pts: Option<gst::ClockTime>,
    pub keyframe: bool,
    pub codec_data: Option<Vec<u8>>,
}
pub struct Capture {
    pipeline: gst::Pipeline,
    sink: AppSink,
    bus: gst::Bus,
}

impl Capture {
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        let caps = self.sink.static_pad("sink")?.current_caps()?;
        let structure = caps.structure(0)?;
        Some((
            u32::try_from(structure.get::<i32>("width").ok()?).ok()?,
            u32::try_from(structure.get::<i32>("height").ok()?).ok()?,
        ))
    }
    /// Map a sample PTS from this pipeline's running clock to the local
    /// monotonic clock, retaining encoding/capture delay for A/V alignment.
    /// Age of a sample relative to the pipeline's running clock. A compositor
    /// reconfiguration can reset PipeWire's timestamp base without an error;
    /// the caller uses an implausible age to detect and recover that case.
    pub fn sample_age(&self, sample: &Sample) -> std::time::Duration {
        self.pipeline
            .current_running_time()
            .zip(sample.pts)
            .and_then(|(running, pts)| running.checked_sub(pts))
            .map(|clock| std::time::Duration::from_nanos(clock.nseconds()))
            .unwrap_or_default()
    }
    pub fn sample_instant(&self, sample: &Sample) -> std::time::Instant {
        let now = std::time::Instant::now();
        now.checked_sub(self.sample_age(sample)).unwrap_or(now)
    }
    fn start(description: &str, audio_device: Option<&str>) -> Result<Self> {
        gst::init()?;
        let pipeline = gst::parse::launch(description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("invalid capture pipeline"))?;
        let sink = pipeline
            .by_name("samples")
            .context("capture missing appsink")?
            .downcast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("invalid appsink"))?;
        if let Some(device) = audio_device {
            pipeline
                .by_name("capture")
                .context("missing audio source")?
                .set_property("device", device);
        }
        let bus = pipeline.bus().context("capture missing bus")?;
        pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            pipeline,
            sink,
            bus,
        })
    }
    /// Use the exact output authorized by `media::Capture`, never enumerate cameras.
    pub fn video(
        source: Source,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        ensure!(
            (16..=7680).contains(&width)
                && (16..=4320).contains(&height)
                && (1..=60).contains(&fps)
                && (128..=50000).contains(&bitrate_kbps),
            "invalid video parameters"
        );
        let input = match source {
            Source::PipeWire { fd, node } => format!(
                "pipewiresrc name=capture fd={fd} path={node} do-timestamp=true keepalive-time={}",
                1000 / fps
            ),
            Source::Test => "videotestsrc name=capture is-live=true pattern=ball".to_owned(),
        };
        let pipeline = format!(
            "{input} ! queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream ! videoconvert ! videoscale add-borders=true ! videorate drop-only=true ! video/x-raw,format=I420,width={width},height={height},framerate={fps}/1 ! x264enc name=video_encoder tune=zerolatency speed-preset=veryfast bitrate={bitrate_kbps} key-int-max={fps} bframes=0 threads=4 ! h264parse config-interval=-1 ! video/x-h264,stream-format=avc,alignment=au ! appsink name=samples sync=false max-buffers=2 drop=false"
        );
        Self::start(&pipeline, None)
    }
    /// Capture only this sink's PulseAudio-compatible monitor, never the default output.
    pub fn audio(monitor: &str) -> Result<Self> {
        ensure!(
            monitor.ends_with(".monitor") && !monitor.contains(['\r', '\n', '\0']),
            "invalid audio monitor"
        );
        Self::start(
            "pulsesrc name=capture do-timestamp=true ! queue max-size-time=100000000 max-size-buffers=0 max-size-bytes=0 leaky=downstream ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=2,layout=interleaved ! appsink name=samples sync=false max-buffers=10 drop=true",
            Some(monitor),
        )
    }
    /// Change the live encoder bitrate without restarting the pipeline.
    pub fn set_bitrate(&self, kbps: u32) -> Result<()> {
        let encoder = self
            .pipeline
            .by_name("video_encoder")
            .context("capture missing video encoder")?;
        encoder.set_property("bitrate", kbps);
        Ok(())
    }

    /// Restart after the compositor reconfigures the captured output. The
    /// portal FD and node are reused; a destroyed stream keeps failing and the
    /// caller decides how long to wait without ending the AirPlay session.
    pub fn recover(&mut self) -> Result<()> {
        self.pipeline.set_state(gst::State::Null)?;
        while self.bus.pop().is_some() {}
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    /// Nonblocking pull; inspect errors even when no new samples are available.
    pub fn pull(&self) -> Result<Option<Sample>> {
        while let Some(message) = self.bus.pop() {
            match message.view() {
                gst::MessageView::Error(error) => {
                    anyhow::bail!("capture error: {} ({:?})", error.error(), error.debug())
                }
                gst::MessageView::Eos(_) => anyhow::bail!("capture ended"),
                _ => {}
            }
        }
        let Some(sample) = self.sink.try_pull_sample(gst::ClockTime::ZERO) else {
            return Ok(None);
        };
        let buffer = sample.buffer().context("sample missing buffer")?;
        let mapped = buffer
            .map_readable()
            .map_err(|_| anyhow::anyhow!("sample cannot be mapped"))?;
        let codec_data = sample
            .caps()
            .and_then(|caps| caps.structure(0))
            .and_then(|s| s.get::<gst::Buffer>("codec_data").ok())
            .map(|b| {
                b.map_readable()
                    .map(|m| m.as_slice().to_vec())
                    .map_err(|_| anyhow::anyhow!("codec data cannot be mapped"))
            })
            .transpose()?;
        Ok(Some(Sample {
            data: mapped.as_slice().to_vec(),
            pts: buffer.pts(),
            keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
            codec_data,
        }))
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_source_produces_avc() {
        let capture = Capture::video(Source::Test, 320, 240, 10, 500).unwrap();
        let start = std::time::Instant::now();
        loop {
            if let Some(frame) = capture.pull().unwrap() {
                assert!(!frame.data.is_empty());
                let decoder = gst::parse::launch("appsrc name=input format=time ! h264parse ! avdec_h264 ! videoconvert ! video/x-raw,format=I420 ! appsink name=output sync=false")
                    .unwrap().downcast::<gst::Pipeline>().unwrap();
                let input = decoder
                    .by_name("input")
                    .unwrap()
                    .downcast::<gstreamer_app::AppSrc>()
                    .unwrap();
                let output = decoder
                    .by_name("output")
                    .unwrap()
                    .downcast::<AppSink>()
                    .unwrap();
                let caps = gst::Caps::builder("video/x-h264")
                    .field("stream-format", "avc")
                    .field("alignment", "au")
                    .field(
                        "codec_data",
                        gst::Buffer::from_slice(frame.codec_data.unwrap()),
                    )
                    .build();
                input.set_caps(Some(&caps));
                decoder.set_state(gst::State::Playing).unwrap();
                input
                    .push_buffer(gst::Buffer::from_slice(frame.data))
                    .unwrap();
                input.end_of_stream().unwrap();
                let decoded = output
                    .try_pull_sample(gst::ClockTime::from_seconds(5))
                    .expect("H.264 decoder produced no frame");
                let info = decoded.caps().unwrap().structure(0).unwrap();
                assert_eq!(info.get::<i32>("width").unwrap(), 320);
                assert_eq!(info.get::<i32>("height").unwrap(), 240);
                decoder.set_state(gst::State::Null).unwrap();
                break;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

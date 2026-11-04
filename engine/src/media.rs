use std::{
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail, ensure};
use ashpd::desktop::{
    PersistMode, Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::AppSink;
use zbus::Connection;

use crate::wfd::VideoMode;

pub struct Capture {
    connection: Connection,
    portal: Screencast,
    session: Session<Screencast>,
    fd: Option<OwnedFd>,
    node: u32,
    mapping: Option<String>,
}

impl Capture {
    pub async fn new() -> Result<Self> {
        // A dedicated bus connection lets cancellation revoke permission even
        // before Start/OpenPipeWireRemote has returned.
        let connection = Connection::session().await?;
        let portal = Screencast::with_connection(connection.clone()).await?;
        let session = portal.create_session(Default::default()).await?;
        Ok(Self {
            connection,
            portal,
            session,
            fd: None,
            node: 0,
            mapping: None,
        })
    }

    pub async fn select(
        &mut self,
        expected_output: &str,
        expected_position: (i32, i32),
    ) -> Result<()> {
        self.select_monitor(Some((expected_output, expected_position)))
            .await
    }

    /// Let the user explicitly choose any monitor in the portal (mirror mode).
    pub async fn select_mirror(&mut self) -> Result<()> {
        self.select_monitor(None).await
    }

    async fn select_monitor(&mut self, expected: Option<(&str, (i32, i32))>) -> Result<()> {
        let modes = self.portal.available_cursor_modes().await?;
        let cursor = if modes.contains(CursorMode::Embedded) {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        };
        self.portal
            .select_sources(
                &self.session,
                SelectSourcesOptions::default()
                    .set_sources(Some(SourceType::Monitor.into()))
                    .set_multiple(false)
                    .set_cursor_mode(cursor)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await?
            .response()?;
        let response = self
            .portal
            .start(&self.session, None, Default::default())
            .await?
            .response()?;
        ensure!(
            response.streams().len() == 1,
            "El portal no entregó exactamente una pantalla"
        );
        let stream = &response.streams()[0];
        if let Some((expected_output, expected_position)) = expected {
            validate_stream(
                expected_output,
                expected_position,
                stream.mapping_id(),
                stream.position(),
                stream.size(),
            )?;
        }
        self.mapping = stream.mapping_id().map(str::to_owned);
        // The portal is a restricted remote: never search the unrestricted
        // PipeWire graph, which could accidentally choose a webcam.
        self.node = stream.pipe_wire_node_id();
        self.fd = Some(
            self.portal
                .open_pipe_wire_remote(&self.session, Default::default())
                .await?,
        );
        Ok(())
    }

    /// Connector name of the monitor the user chose in the portal, when the
    /// compositor reports it (Hyprland does). Used to release an unused virtual
    /// output when the user chooses to mirror an existing monitor instead.
    pub fn mapping(&self) -> Option<&str> {
        self.mapping.as_deref()
    }

    pub fn source(&self) -> Result<Source> {
        Ok(Source::PipeWire {
            fd: self
                .fd
                .as_ref()
                .context("El portal no autorizó la captura")?
                .as_raw_fd(),
            node: self.node,
        })
    }

    pub async fn close(self) -> Result<()> {
        let _ = self.session.close().await;
        self.connection.close().await?;
        Ok(())
    }
}

fn validate_stream(
    expected_output: &str,
    expected_position: (i32, i32),
    mapping_id: Option<&str>,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
) -> Result<()> {
    // Hyprland reports position=(0,0) even for secondary outputs. Its
    // mapping_id contains the actual connector name and takes precedence.
    if let Some(selected) = mapping_id.filter(|id| !id.is_empty()) {
        ensure!(
            selected == expected_output,
            "Se eligió {selected}; selecciona {expected_output} para extender el escritorio"
        );
    } else {
        // Fail closed for incomplete/legacy metadata; never silently capture
        // an unrelated desktop just because it also has a 1080p mode.
        ensure!(
            position == Some(expected_position) && size == Some((1920, 1080)),
            "El portal no identifica el monitor virtual seleccionado. Se esperaba {expected_output}; no se enviará otra pantalla"
        );
    }
    if let Some(size) = size {
        ensure!(
            size == (1920, 1080),
            "La pantalla elegida no es el monitor virtual de 1920×1080"
        );
    }
    Ok(())
}

pub enum Source {
    Test,
    PipeWire { fd: i32, node: u32 },
}

pub fn audio_available() -> bool {
    ["pulsesrc", "audioconvert", "audioresample", "aacparse"]
        .iter()
        .all(|name| gst::ElementFactory::find(name).is_some())
        && audio_encoder().is_some()
}

fn audio_encoder() -> Option<&'static str> {
    ["fdkaacenc", "avenc_aac", "faac"]
        .into_iter()
        .find(|name| gst::ElementFactory::find(name).is_some())
}

fn default_output_monitor() -> Result<String> {
    let output = std::process::Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .context("No se pudo consultar la salida de audio con pactl")?;
    ensure!(
        output.status.success(),
        "No se pudo consultar la salida de audio predeterminada"
    );
    let name = std::str::from_utf8(&output.stdout)?.trim();
    ensure!(
        !name.is_empty() && !name.contains('\n'),
        "Salida de audio predeterminada inválida"
    );
    Ok(format!("{name}.monitor"))
}

pub struct Video {
    pipeline: gst::Pipeline,
    sink: AppSink,
    encoder: gst::Element,
    bus: gst::Bus,
    captured: Arc<AtomicU64>,
    encoded: Arc<AtomicU64>,
    produced: Arc<AtomicU64>,
}

impl Video {
    pub fn start(mode: &VideoMode, source: Source, kbps: u32, audio: bool) -> Result<Self> {
        let test = matches!(source, Source::Test);
        let source = match source {
            Source::Test => "videotestsrc name=capture is-live=true pattern=ball".to_string(),
            Source::PipeWire { fd, node } => format!(
                "pipewiresrc name=capture fd={fd} path={node} do-timestamp=true keepalive-time={}",
                1000 / mode.fps
            ),
        };
        let audio_branch = if audio {
            let input = if test {
                "audiotestsrc name=audio_capture is-live=true wave=sine"
            } else {
                "pulsesrc name=audio_capture do-timestamp=true"
            };
            let encoder = audio_encoder().context("No hay codificador AAC disponible")?;
            format!(
                " {input} ! queue max-size-time=500000000 max-size-buffers=0 max-size-bytes=0 ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! {encoder} ! aacparse ! audio/mpeg,mpegversion=4,stream-format=adts ! queue ! mux.sink_4352"
            )
        } else {
            String::new()
        };
        let pipeline_text = format!(
            "{source} ! queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! videoconvert ! videoscale add-borders=true ! videorate drop-only=true \
             ! video/x-raw,format=I420,width={},height={},framerate={}/1 \
             ! x264enc name=encoder tune=zerolatency speed-preset=veryfast bitrate={kbps} \
               key-int-max={} bframes=0 byte-stream=true aud=true vbv-buf-capacity=200 \
             ! video/x-h264,profile=(string){},level=(string){},stream-format=byte-stream,alignment=au \
             ! h264parse config-interval=-1 ! mux.sink_4113 \
             mpegtsmux name=mux alignment=7 pat-interval=9000 pmt-interval=9000 pcr-interval=1800 \
             ! rtpmp2tpay pt=33 mtu=1400 ! appsink name=packets sync=true max-buffers=16 drop=false {audio_branch}",
            mode.width,
            mode.height,
            mode.fps,
            mode.fps,
            mode.gst_profile(),
            mode.gst_level(),
        );
        let pipeline = gst::parse::launch(&pipeline_text)
            .context("No se pudo construir el codificador H.264")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("GStreamer no devolvió una pipeline"))?;
        let encoder = pipeline.by_name("encoder").context("Falta x264enc")?;
        let sink = pipeline
            .by_name("packets")
            .context("Falta appsink")?
            .downcast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("appsink inválido"))?;
        if audio && !test {
            pipeline
                .by_name("audio_capture")
                .context("Falta la captura de audio")?
                .set_property("device", default_output_monitor()?);
        }
        // WFD elementary PIDs: video 0x1011 and audio 0x1100.
        pipeline
            .by_name("mux")
            .context("Falta mpegtsmux")?
            .set_property(
                "prog-map",
                gst::Structure::builder("program_map")
                    .field("sink_4113", 1i32)
                    .field("sink_4352", 1i32)
                    .field("PMT_1", 0x1000u32)
                    .field("PCR_1", "sink_4113")
                    .build(),
            );
        let bus = pipeline.bus().context("Falta el bus de GStreamer")?;
        let captured = Arc::new(AtomicU64::new(0));
        let encoded = Arc::new(AtomicU64::new(0));
        let produced = Arc::new(AtomicU64::new(0));
        for (element, pad, counter) in [
            ("capture", "src", &captured),
            ("encoder", "src", &encoded),
            ("packets", "sink", &produced),
        ] {
            let pad = pipeline
                .by_name(element)
                .with_context(|| format!("Falta {element}"))?
                .static_pad(pad)
                .context("Falta pad de diagnóstico")?;
            let count = counter.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                count.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            })
            .context("No se pudo instalar contador de diagnóstico")?;
        }
        let video = Self {
            pipeline,
            sink,
            encoder,
            bus,
            captured,
            encoded,
            produced,
        };
        video
            .pipeline
            .set_state(gst::State::Playing)
            .context("No se pudo iniciar la codificación")?;
        Ok(video)
    }

    pub fn packet(&self) -> Result<Option<Vec<u8>>> {
        while let Some(message) = self.bus.pop() {
            match message.view() {
                gst::MessageView::Error(error) => bail!(
                    "Captura/codificación: {} ({})",
                    error.error(),
                    error.debug().unwrap_or_default()
                ),
                gst::MessageView::Eos(..) => bail!("La captura de pantalla terminó"),
                _ => {}
            }
        }
        let Some(sample) = self.sink.try_pull_sample(gst::ClockTime::ZERO) else {
            return Ok(None);
        };
        let map = sample.buffer().context("Fotograma vacío")?.map_readable()?;
        ensure!(
            map.len() >= 12 && map[0] >> 6 == 2 && map[1] & 0x7f == 33,
            "RTP MPEG-TS inválido"
        );
        Ok(Some(map.to_vec()))
    }

    pub fn set_bitrate(&self, kbps: u32) {
        self.encoder.set_property("bitrate", kbps);
    }

    pub fn counters(&self) -> (u64, u64, u64) {
        (
            self.captured.load(Ordering::Relaxed),
            self.encoded.load(Ordering::Relaxed),
            self.produced.load(Ordering::Relaxed),
        )
    }

    #[cfg(test)]
    pub fn bitrate(&self) -> u32 {
        self.encoder.property("bitrate")
    }

    pub fn request_keyframe(&self) {
        let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        let _ = self.encoder.send_event(event);
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Hardware-free smoke test of the real encoder + muxer + RTP payloader.
pub async fn self_test() -> Result<serde_json::Value> {
    let mode = VideoMode {
        width: 1920,
        height: 1080,
        fps: 30,
        cea_bit: 1 << 7,
        profile: 2,
        level: 4,
    };
    let video = Video::start(&mode, Source::Test, 5000, false)?;
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut packets = 0u64;
    let mut bytes = 0u64;
    let mut adjusted = false;
    while tokio::time::Instant::now() < end {
        if !adjusted && packets > 10 {
            video.set_bitrate(3000);
            video.request_keyframe();
            adjusted = true;
        }
        if let Some(packet) = video.packet()? {
            // No CSRCs or RTP extensions are configured in our payloader.
            ensure!(
                (packet.len() - 12) % 188 == 0,
                "RTP no contiene paquetes TS completos"
            );
            ensure!(
                packet[12..].chunks(188).all(|p| p[0] == 0x47),
                "MPEG-TS sin sincronización"
            );
            packets += 1;
            bytes += packet.len() as u64;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    ensure!(
        packets >= 30,
        "El codificador produjo muy pocos paquetes: {packets}"
    );
    ensure!(adjusted, "No se pudo probar el cambio de bitrate");
    Ok(
        serde_json::json!({"ok":true,"width":1920,"height":1080,"fps":30,"packets":packets,"bytes":bytes,"bitrate_changed":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyprland_mapping_id_overrides_placeholder_position() {
        assert!(
            validate_stream(
                "OMARCHY-WFD-42",
                (1536, 0),
                Some("OMARCHY-WFD-42"),
                Some((0, 0)),
                Some((1920, 1080))
            )
            .is_ok()
        );
    }

    #[test]
    fn wrong_output_is_rejected_even_when_geometry_matches() {
        assert!(
            validate_stream(
                "OMARCHY-WFD-42",
                (1536, 0),
                Some("eDP-1"),
                Some((1536, 0)),
                Some((1920, 1080))
            )
            .is_err()
        );
    }

    #[test]
    fn incomplete_or_wrong_size_metadata_does_not_authorize_another_screen() {
        assert!(validate_stream("OMARCHY-WFD-42", (1536, 0), None, None, None).is_err());
        assert!(
            validate_stream(
                "OMARCHY-WFD-42",
                (1536, 0),
                None,
                Some((0, 0)),
                Some((1920, 1080))
            )
            .is_err()
        );
        assert!(
            validate_stream(
                "OMARCHY-WFD-42",
                (1536, 0),
                Some("OMARCHY-WFD-42"),
                Some((0, 0)),
                Some((3840, 2160))
            )
            .is_err()
        );
    }
}

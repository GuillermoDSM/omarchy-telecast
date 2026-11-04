//! Media loop for an established session and portal-authorised capture.
use super::{
    capture::Capture,
    event::Event,
    session::Session,
    sources::{Kind as SourceKind, VideoSources},
};
use crate::audio::TvOutput;
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval, timeout},
};
use tokio_util::sync::CancellationToken;

/// Keeps the output alive, checks its health, and closes the AirPlay session
/// before returning. Caller keeps the portal authorization and display alive.
pub async fn run(
    mut session: Session,
    mut sources: VideoSources,
    audio: Capture,
    output: &TvOutput,
    mut receiver_events: mpsc::Receiver<Event>,
    cancel: CancellationToken,
    events: crate::Events,
) -> Result<()> {
    let mut polling = interval(Duration::from_millis(3));
    polling.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut audio_tick = interval(Duration::from_nanos(352_000_000_000 / 44_100));
    audio_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut control_tick = interval(Duration::from_secs(1));
    control_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut pcm = VecDeque::with_capacity(14080);
    let mut rtp = 0u32;
    let mut codec: Option<Vec<u8>> = None;
    let mut waiting_for_keyframe = true;
    let mut frames_sent = 0u64;
    let mut capture_interrupted: Option<Instant> = None;
    let audio_start = Instant::now();
    let mut layout = crate::display::layout_signature().await.ok();
    let result: Result<()> = async {
        let (timestamp,timeline) = session.network_time(Instant::now())?;
        session.audio.sync(timestamp,if timeline == 0 { None } else { Some(timeline) },0,session.audio_latency_samples,true).await?;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = receiver_events.recv(), if !receiver_events.is_closed() => {
                    if let Some(command) = command { session.handle_event(&command)?; }
                }
                retransmit = session.audio.receive_retransmit() => {
                    retransmit.context("error de retransmisión de audio")?;
                }
                _ = polling.tick() => {
                    for _ in 0..4 {
                        let frame = match sources.capture_mut().pull() {
                            Ok(Some(frame)) => {
                                capture_interrupted = None;
                                frame
                            }
                            Ok(None) => break,
                            Err(_) => {
                                // Mirroring or disabling an output makes the
                                // portal destroy its screencopy stream. Keep the
                                // AirPlay session and receiver feedback alive and
                                // rebuild the capture pipeline instead of
                                // dropping the TV connection.
                                let retry = capture_interrupted.is_none_or(|at| at.elapsed() >= Duration::from_secs(2));
                                if capture_interrupted.is_none() {
                                    events.trace("native_media", "capture_interrupted");
                                    events.warning("La captura se interrumpió por un cambio de pantalla; AirPlay sigue conectado y se reintentará la captura");
                                }
                                if retry {
                                    let _ = sources.capture_mut().recover();
                                }
                                capture_interrupted = Some(Instant::now());
                                waiting_for_keyframe = true;
                                codec = None;
                                break;
                            }
                        };
                        // A mirror/extended toggle can reset the PipeWire
                        // timestamp base without reporting an error. Detect the
                        // implausible age, rebuild the pipeline and wait for a
                        // fresh keyframe instead of sending late frames forever.
                        if sources.capture_mut().sample_age(&frame) > Duration::from_millis(750) {
                            let retry = capture_interrupted
                                .is_none_or(|at| at.elapsed() >= Duration::from_secs(2));
                            if capture_interrupted.is_none() {
                                events.trace("native_media", "capture_timestamp_reset");
                                events.warning("Hyprland cambió la pantalla; AirPlay sigue conectado y se está reiniciando la captura");
                            }
                            if retry {
                                let _ = sources.capture_mut().recover();
                            }
                            capture_interrupted = Some(Instant::now());
                            waiting_for_keyframe = true;
                            codec = None;
                            continue;
                        }
                        capture_interrupted = None;
                        let Some(configuration) = frame.codec_data.as_ref() else { continue };
                        if waiting_for_keyframe && !frame.keyframe { continue; }
                        let captured = sources.capture_mut().sample_instant(&frame);
                        let (clock,timeline) = session.network_time(captured)?;
                        let timestamp = clock.checked_add(super::timing::compact_time(Duration::from_millis(u64::from(session.video_latency_ms)))).context("video timestamp overflow")?;
                        if codec.as_ref() != Some(configuration) {
                            // Dimensions are read from negotiated H.264 caps by the caller's fixed capture configuration.
                            let (width,height) = sources.capture_mut().dimensions().context("missing negotiated video dimensions")?;
                            timeout(Duration::from_secs(2),session.video.codec(configuration,width,height,timestamp)).await.context("Tiempo de configuración de vídeo agotado")??;
                            codec = Some(configuration.clone());
                        }
                        timeout(Duration::from_secs(2),session.video.frame(&frame.data,frame.keyframe,timestamp,timeline)).await.context("Tiempo de envío de vídeo agotado")??;
                        if waiting_for_keyframe { events.trace("native_media", "first_keyframe_sent"); waiting_for_keyframe = false; }
                        frames_sent += 1;
                    }
                    for _ in 0..8 {
                        let Some(chunk) = audio.pull()? else { break };
                        ensure!(chunk.data.len() % 4 == 0,"unaligned captured PCM");
                        // The appsink can retain samples while a sink is idle or
                        // the network is stalled. Do not replay them on return.
                        if audio.sample_instant(&chunk).elapsed() > Duration::from_millis(200) {
                            continue;
                        }
                        let start = chunk.data.len().saturating_sub(14080);
                        pcm.extend(&chunk.data[start..]);
                        // Never play stale backlog after the sender falls behind.
                        while pcm.len() > 14080 { pcm.pop_front(); }
                    }
                }
                _ = audio_tick.tick() => {
                    let elapsed = audio_start.elapsed().as_nanos();
                    let current = (((elapsed * 44_100 / 1_000_000_000) / 352) * 352) as u32;
                    if current.wrapping_sub(rtp) > 352 && current.wrapping_sub(rtp) < u32::MAX / 2 { pcm.clear(); rtp = current; }
                    let mut frame = [0u8;1408];
                    for byte in &mut frame { if let Some(value) = pcm.pop_front() { *byte = value; } else { break; } }
                    session.send_pcm(&frame,rtp).await?; rtp = rtp.wrapping_add(352);
                }
                _ = control_tick.tick() => {
                    if let Ok(monitors) = crate::display::monitors().await {
                        let current_layout = crate::display::signature(&monitors);
                        if Some(current_layout.clone()) != layout {
                            layout = Some(current_layout);
                            events.trace("native_media", "monitor_layout_changed");
                            let mirrored = sources
                                .virtual_name()
                                .and_then(|name| monitors.iter().find(|monitor| monitor.name == name))
                                .is_some_and(|monitor| {
                                    !monitor.mirror_of.is_empty() && monitor.mirror_of != "none"
                                });
                            let target = if !sources.has_virtual() {
                                None
                            } else if mirrored {
                                Some(SourceKind::Laptop)
                            } else if sources.active() == SourceKind::Laptop {
                                Some(SourceKind::Virtual)
                            } else {
                                None
                            };
                            let switched = match target {
                                Some(SourceKind::Laptop) => {
                                    events.warning("Cambiando a duplicar: selecciona la pantalla de la laptop en el portal");
                                    Some(sources.select_laptop(&events).await)
                                }
                                Some(SourceKind::Virtual) => Some(
                                    sources
                                        .select_virtual()
                                        .context("No se pudo reactivar la captura extendida"),
                                ),
                                None => None,
                            };
                            match switched {
                                Some(Err(_)) => {
                                    events.warning(
                                        "Cambió la disposición de pantallas; AirPlay sigue conectado, pero la captura no pudo cambiarse",
                                    );
                                    let _ = sources.capture_mut().recover();
                                }
                                None => {
                                    let _ = sources.capture_mut().recover();
                                }
                                Some(Ok(())) => {}
                            }
                            waiting_for_keyframe = true;
                            codec = None;
                            capture_interrupted = Some(Instant::now());
                        }
                    }
                    events.send(serde_json::json!({"event":"trace","stage":"native_media","detail":"counters","video_frames":frames_sent,"codec_config_sent":codec.is_some()}));
                    output.check_pulse_monitor().await?;
                    let (timestamp,timeline) = session.network_time(Instant::now())?;
                    session.audio.sync(timestamp,if timeline == 0 { None } else { Some(timeline) },rtp,session.audio_latency_samples,false).await?;
                    timeout(Duration::from_secs(2),session.feedback()).await.context("Tiempo de feedback AirPlay agotado")??;
                }
            }
        }
        Ok(())
    }.await;
    let stop = timeout(Duration::from_secs(2), session.stop())
        .await
        .context("Tiempo de cierre AirPlay agotado")
        .and_then(|result| result);
    sources.close().await;
    result.and(stop)
}

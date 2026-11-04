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
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval, timeout},
};
use tokio_util::sync::CancellationToken;

enum VideoCommand {
    Codec {
        avcc: Vec<u8>,
        width: u32,
        height: u32,
        timestamp: u64,
    },
    Frame {
        data: Vec<u8>,
        keyframe: bool,
        timestamp: u64,
        timeline: u64,
    },
}

/// Keeps the output alive, checks its health, and closes the AirPlay session
/// before returning. Caller keeps the portal authorization and display alive.
pub async fn run(
    mut session: Session,
    mut sources: VideoSources,
    audio: Capture,
    _output: &TvOutput,
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
    let mut capture_age_max = 0u64;
    let mut late_frames = 0u64;
    let mut bitrate_kbps = crate::airplay_native::capture::DEFAULT_BITRATE_KBPS;
    let mut clean_intervals = 0u32;
    let audio_start = Instant::now();
    let mut layout = crate::display::layout_signature().await.ok();
    let mut video_sender = session.take_video()?;
    let (video_tx, mut video_rx) = mpsc::channel::<VideoCommand>(4);
    let (video_error_tx, mut video_error_rx) = mpsc::channel::<String>(1);
    let send_blocked = Arc::new(AtomicU64::new(0));
    let blocked_counter = send_blocked.clone();
    let video_task = tokio::spawn(async move {
        while let Some(command) = video_rx.recv().await {
            let started = Instant::now();
            let result = match command {
                VideoCommand::Codec {
                    avcc,
                    width,
                    height,
                    timestamp,
                } => timeout(
                    Duration::from_secs(2),
                    video_sender.codec(&avcc, width, height, timestamp),
                )
                .await
                .context("Tiempo de configuración de vídeo agotado")
                .and_then(|result| result),
                VideoCommand::Frame {
                    data,
                    keyframe,
                    timestamp,
                    timeline,
                } => timeout(
                    Duration::from_secs(2),
                    video_sender.frame(&data, keyframe, timestamp, timeline),
                )
                .await
                .context("Tiempo de envío de vídeo agotado")
                .and_then(|result| result),
            };
            blocked_counter.fetch_max(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            if let Err(error) = result {
                let _ = video_error_tx.send(format!("{error:#}"));
                return;
            }
        }
    });
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
                video_error = video_error_rx.recv(), if !video_error_rx.is_closed() => {
                    if let Some(error) = video_error {
                        return Err(anyhow::anyhow!(error));
                    }
                }
                _ = polling.tick() => {
                    for _ in 0..4 {
                        // Never pull a frame unless the video task can accept
                        // both a possible codec update and the frame. Dropping
                        // encoded P-frames would corrupt the H.264 chain.
                        if video_tx.capacity() < 2 {
                            break;
                        }
                        let mut frame = match sources.capture_mut().pull() {
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
                        let age = sources.capture_mut().sample_age(&frame);
                        if age > Duration::from_millis(750) {
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
                        let captured = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
                        let (clock,timeline) = session.network_time(captured)?;
                        let timestamp = clock.checked_add(super::timing::compact_time(Duration::from_millis(u64::from(session.video_latency_ms)))).context("video timestamp overflow")?;
                        if codec.as_ref() != Some(configuration) {
                            // Dimensions are read from negotiated H.264 caps by the caller's fixed capture configuration.
                            let (width,height) = sources.capture_mut().dimensions().context("missing negotiated video dimensions")?;
                            video_tx
                                .try_send(VideoCommand::Codec {
                                    avcc: configuration.clone(),
                                    width,
                                    height,
                                    timestamp,
                                })
                                .map_err(|_| anyhow::anyhow!("video task stopped"))?;
                            codec = Some(configuration.clone());
                        }
                        let data = std::mem::take(&mut frame.data);
                        video_tx
                            .try_send(VideoCommand::Frame {
                                data,
                                keyframe: frame.keyframe,
                                timestamp,
                                timeline,
                            })
                            .map_err(|_| anyhow::anyhow!("video task stopped"))?;
                        capture_age_max = capture_age_max.max(age.as_millis() as u64);
                        if age > Duration::from_millis(u64::from(session.video_latency_ms)) {
                            late_frames += 1;
                        }
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
                    let send_blocked_max = send_blocked.swap(0, Ordering::Relaxed);
                    // Keep 8 Mb/s when the link is healthy. Back off only on
                    // real sender congestion, then recover gradually. Lowering
                    // quality preemptively would trade image quality for no
                    // benefit on a clean Wi-Fi link.
                    if send_blocked_max >= 20 || late_frames > 0 {
                        clean_intervals = 0;
                        let next = (bitrate_kbps * 3 / 4).max(3000);
                        if next < bitrate_kbps
                            && sources.capture_mut().set_bitrate(next).is_ok()
                        {
                            bitrate_kbps = next;
                            events.trace("native_media", "bitrate_backoff");
                        }
                    } else if send_blocked_max <= 2 && capture_age_max <= 50 {
                        clean_intervals += 1;
                        if clean_intervals >= 10 {
                            clean_intervals = 0;
                            let next = (bitrate_kbps + 500).min(
                                crate::airplay_native::capture::DEFAULT_BITRATE_KBPS,
                            );
                            if next > bitrate_kbps
                                && sources.capture_mut().set_bitrate(next).is_ok()
                            {
                                bitrate_kbps = next;
                                events.trace("native_media", "bitrate_recover");
                            }
                        }
                    } else {
                        clean_intervals = 0;
                    }
                    events.send(serde_json::json!({
                        "event":"trace","stage":"native_media","detail":"counters",
                        "video_frames":frames_sent,
                        "codec_config_sent":codec.is_some(),
                        "capture_age_max_ms":capture_age_max,
                        "send_blocked_max_ms":send_blocked_max,
                        "late_frames":late_frames,
                        "bitrate_kbps":bitrate_kbps
                    }));
                    capture_age_max = 0;
                    late_frames = 0;
                    // The pulse monitor is checked by the audio pipeline itself;
                    // running pactl/pw-dump here would block audio pacing.
                    let (timestamp,timeline) = session.network_time(Instant::now())?;
                    session.audio.sync(timestamp,if timeline == 0 { None } else { Some(timeline) },rtp,session.audio_latency_samples,false).await?;
                    timeout(Duration::from_secs(2),session.feedback()).await.context("Tiempo de feedback AirPlay agotado")??;
                }
            }
        }
        Ok(())
    }.await;
    drop(video_tx);
    let _ = timeout(Duration::from_secs(2), video_task).await;
    let stop = timeout(Duration::from_secs(2), session.stop())
        .await
        .context("Tiempo de cierre AirPlay agotado")
        .and_then(|result| result);
    sources.close().await;
    result.and(stop)
}

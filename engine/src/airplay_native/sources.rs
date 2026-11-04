//! Capture source switching for AirPlay.
//!
//! Hyprland's screencopy portal does not deliver frames for a headless output
//! while that output is mirrored. Extended mode therefore captures the virtual
//! output, while mirror mode captures the physical laptop monitor. The second
//! portal session is requested lazily the first time mirror is used, then kept
//! alive so later toggles are immediate and never interrupt AirPlay.
use super::capture::Capture as VideoCapture;
use crate::{Events, display, media};
use anyhow::{Context, Result, ensure};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Virtual,
    Laptop,
}

pub struct VideoSources {
    virtual_name: Option<String>,
    virtual_portal: Option<media::Capture>,
    virtual_capture: Option<VideoCapture>,
    laptop_portal: Option<media::Capture>,
    laptop_capture: Option<VideoCapture>,
    active: Kind,
}

impl VideoSources {
    pub fn new(
        virtual_name: Option<String>,
        portal: media::Capture,
        capture: VideoCapture,
        active: Kind,
    ) -> Self {
        let (virtual_portal, virtual_capture, laptop_portal, laptop_capture) = match active {
            Kind::Virtual => (Some(portal), Some(capture), None, None),
            Kind::Laptop => (None, None, Some(portal), Some(capture)),
        };
        Self {
            virtual_name,
            virtual_portal,
            virtual_capture,
            laptop_portal,
            laptop_capture,
            active,
        }
    }
    pub fn test(capture: VideoCapture) -> Self {
        Self {
            virtual_name: None,
            virtual_portal: None,
            virtual_capture: Some(capture),
            laptop_portal: None,
            laptop_capture: None,
            active: Kind::Virtual,
        }
    }
    pub fn active(&self) -> Kind {
        self.active
    }
    pub fn virtual_name(&self) -> Option<&str> {
        self.virtual_name.as_deref()
    }
    pub fn has_virtual(&self) -> bool {
        self.virtual_capture.is_some()
    }
    pub fn capture_mut(&mut self) -> &mut VideoCapture {
        match self.active {
            Kind::Virtual => self.virtual_capture.as_mut().expect("virtual capture missing"),
            Kind::Laptop => self.laptop_capture.as_mut().expect("laptop capture missing"),
        }
    }
    /// Rebuild the virtual pipeline after a mirror cycle and select it again.
    pub fn select_virtual(&mut self) -> Result<()> {
        let capture = self
            .virtual_capture
            .as_mut()
            .context("la sesión no tiene captura del monitor virtual")?;
        capture.recover()?;
        self.active = Kind::Virtual;
        Ok(())
    }
    /// Authorize (once per session) and select the physical laptop monitor.
    pub async fn select_laptop(&mut self, events: &Events) -> Result<()> {
        if self.laptop_capture.is_none() {
            ensure!(
                self.virtual_name.is_some(),
                "el modo duplicar requiere una sesión con monitor virtual"
            );
            let monitors = display::monitors().await?;
            let laptop = monitors
                .iter()
                .find(|monitor| {
                    !monitor.disabled
                        && !monitor.name.starts_with("OMARCHY-WFD-")
                        && !monitor.name.is_empty()
                })
                .context("No hay una pantalla física para duplicar")?;
            events.warning(
                "Selecciona la pantalla de la laptop en el portal para duplicarla; AirPlay sigue conectado",
            );
            let mut portal = media::Capture::new().await?;
            // The selector is restricted to the physical monitor so a wrong
            // choice fails closed instead of capturing an unrelated output.
            portal
                .select(&laptop.name, (laptop.x, laptop.y))
                .await
                .context("No se autorizó la pantalla de la laptop")?;
            let source = portal.source()?;
            let capture = VideoCapture::video(source, 1920, 1080, 30, 4000)?;
            // Use the same bounded startup wait as the initial source.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while capture.pull()?.is_none() {
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "La captura de la laptop no produjo imágenes"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            self.laptop_portal = Some(portal);
            self.laptop_capture = Some(capture);
        }
        self.active = Kind::Laptop;
        Ok(())
    }
    /// Close both portal sessions explicitly. Pipelines close on drop.
    pub async fn close(&mut self) {
        self.virtual_capture.take();
        self.laptop_capture.take();
        if let Some(portal) = self.virtual_portal.take() {
            let _ = portal.close().await;
        }
        if let Some(portal) = self.laptop_portal.take() {
            let _ = portal.close().await;
        }
    }
}

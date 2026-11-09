use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::{Context, Result, ensure};
use ashpd::desktop::{
    PersistMode, Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use zbus::Connection;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyprland_mapping_id_overrides_placeholder_position() {
        assert!(
            validate_stream(
                "OMARCHY-TELECAST-42",
                (1536, 0),
                Some("OMARCHY-TELECAST-42"),
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
                "OMARCHY-TELECAST-42",
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
        assert!(validate_stream("OMARCHY-TELECAST-42", (1536, 0), None, None, None).is_err());
        assert!(
            validate_stream(
                "OMARCHY-TELECAST-42",
                (1536, 0),
                None,
                Some((0, 0)),
                Some((1920, 1080))
            )
            .is_err()
        );
        assert!(
            validate_stream(
                "OMARCHY-TELECAST-42",
                (1536, 0),
                Some("OMARCHY-TELECAST-42"),
                Some((0, 0)),
                Some((3840, 2160))
            )
            .is_err()
        );
    }
}

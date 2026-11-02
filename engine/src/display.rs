use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use tokio::{
    process::Command,
    time::{Duration, timeout},
};

#[derive(Deserialize, Debug, PartialEq)]
pub struct Monitor {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    #[serde(rename = "refreshRate")]
    pub refresh_rate: f64,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default, rename = "mirrorOf")]
    pub mirror_of: String,
}

pub struct VirtualDisplay {
    pub name: String,
    pub position: (i32, i32),
    id: Option<u32>,
    armed: bool,
}

async fn hypr(args: &[&str]) -> Result<String> {
    let mut command = Command::new("hyprctl");
    command.args(args).kill_on_drop(true);
    let output = timeout(Duration::from_secs(4), command.output())
        .await
        .context("Hyprland no respondió")??;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    ensure!(
        output.status.success(),
        "hyprctl: {} {}",
        stdout.trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    // hyprctl eval can return an application error with exit status 0.
    if args.first() != Some(&"-j") {
        ensure!(stdout.trim() == "ok", "hyprctl: {}", stdout.trim());
    }
    Ok(stdout)
}

pub async fn monitors() -> Result<Vec<Monitor>> {
    serde_json::from_str(&hypr(&["-j", "monitors", "all"]).await?)
        .context("Respuesta de monitores inválida")
}

impl VirtualDisplay {
    pub async fn create() -> Result<Self> {
        let name = format!("OMARCHY-WFD-{}", std::process::id());
        let existing = monitors().await?;
        ensure!(
            !existing.iter().any(|m| m.name == name),
            "Ya existe {name}; no se tomará control de una salida ajena"
        );
        ensure!(
            existing.iter().any(|m| !m.disabled),
            "No hay una pantalla disponible"
        );
        let mut display = Self {
            name,
            position: (0, 0),
            id: None,
            armed: true,
        };
        // Configure before hotplug so the output is born at the right mode.
        // Fixed coordinates would push existing position=auto physical outputs
        // to the right; keep this output automatic as well.
        let expr = format!(
            "hl.monitor({{ output = \"{}\", mode = \"1920x1080@30\", position = \"auto-right\", scale = 1 }})",
            display.name
        );
        hypr(&["eval", &expr]).await?;
        hypr(&["output", "create", "headless", &display.name]).await?;
        let output = monitors()
            .await?
            .into_iter()
            .find(|m| m.name == display.name)
            .context("Hyprland no creó la salida virtual")?;
        display.id = Some(output.id);
        display.position = (output.x, output.y);
        ensure!(
            output.width == 1920
                && output.height == 1080
                && !output.disabled
                && (output.scale - 1.0).abs() < 0.01
                && (output.refresh_rate - 30.0).abs() < 0.1,
            "La salida virtual no quedó a 1080p/30 Hz y escala 1"
        );
        Ok(display)
    }

    pub async fn remove(mut self) -> Result<()> {
        let result = remove_owned(&self.name, self.id).await;
        self.armed = false;
        result
    }
}

async fn remove_owned(name: &str, id: Option<u32>) -> Result<()> {
    let current = monitors().await?;
    if !current
        .iter()
        .any(|m| m.name == name && id.is_none_or(|id| m.id == id))
    {
        return Ok(());
    }
    let fallback = current
        .iter()
        .find(|m| !m.disabled && m.name != name && !m.name.starts_with("OMARCHY-WFD-"))
        .context("No se elimina el monitor virtual porque no hay otra pantalla disponible")?;
    ensure!(
        fallback
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "Nombre de monitor no compatible con la API Lua"
    );
    let workspaces: Vec<serde_json::Value> =
        serde_json::from_str(&hypr(&["-j", "workspaces"]).await?)?;
    for workspace in workspaces {
        if workspace["monitor"].as_str() != Some(name) {
            continue;
        }
        let Some(id) = workspace["id"].as_i64() else {
            continue;
        };
        if id > 0 && workspace["windows"].as_u64().unwrap_or(0) > 0 {
            let expr = format!(
                "hl.dispatch(hl.dsp.workspace.move({{ workspace = \"{id}\", monitor = \"{}\" }}))",
                fallback.name
            );
            hypr(&["eval", &expr]).await?;
        }
    }
    // Disabled or mirrored headless outputs are not found by `output remove`.
    // Only re-enable/unmirror the output that belongs to this session.
    if current.iter().any(|m| {
        m.name == name && (m.disabled || (!m.mirror_of.is_empty() && m.mirror_of != "none"))
    }) {
        let expr = format!(
            "hl.monitor({{ output = \"{name}\", disabled = false, mirror = \"\", mode = \"1920x1080@30\", position = \"auto-right\", scale = 1 }})"
        );
        hypr(&["eval", &expr]).await?;
    }
    hypr(&["output", "remove", name]).await?;
    if monitors().await?.iter().any(|m| m.name == name) {
        bail!("Hyprland no retiró {name}");
    }
    Ok(())
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        // Cooperative cleanup is preferred. Also cover a session-task panic,
        // without ever removing another process's or a physical output.
        if self.armed
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let name = self.name.clone();
            let id = self.id;
            runtime.spawn(async move {
                let _ = remove_owned(&name, id).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "Creates and removes one temporary output in the current Hyprland session"]
    async fn local_display_lifecycle() {
        let before = monitors().await.unwrap();
        let display = VirtualDisplay::create().await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let during = monitors().await.unwrap();
        let properties_ok = during.iter().any(|m| {
            m.name == display.name
                && m.width == 1920
                && m.height == 1080
                && (m.scale - 1.0).abs() < 0.01
                && (m.refresh_rate - 30.0).abs() < 0.1
        });
        let physical_unchanged = before.iter().all(|m| during.contains(m));
        display.remove().await.unwrap();
        let after = monitors().await.unwrap();
        assert!(properties_ok);
        assert!(
            physical_unchanged,
            "Creating the headless output changed another monitor"
        );
        assert_eq!(before, after);
    }
}

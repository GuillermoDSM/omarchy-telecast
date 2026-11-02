//! Bounded, private per-session diagnostics. Never record RTSP bodies,
//! device names, addresses, credentials or screen contents.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        fs::MetadataExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct SessionLog {
    file: File,
    pub path: PathBuf,
    records: usize,
    last_metrics: u64,
}

fn directory(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = parent.join(name);
    match fs::create_dir(&path) {
        Ok(()) => fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Directorio de registros inseguro"
    );
    // SAFETY: geteuid reads the process effective UID and has no preconditions.
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "El directorio de registros debe pertenecer al usuario y ser privado (0700)"
    );
    Ok(path)
}

impl SessionLog {
    pub fn open() -> Result<Self> {
        let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME no definido")?)
                .join(".local/state"),
        };
        ensure!(base.is_absolute(), "XDG_STATE_HOME debe ser absoluto");
        // The standard XDG state directory is user managed. Do not recursively
        // change its permissions or follow a plugin-specific symlink.
        let root = directory(&base, "omarchy-miracast")?;
        let logs = directory(&root, "sessions")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        for index in 0..100u8 {
            let path = logs.join(format!(
                "session-{now}-{}-{index}.jsonl",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        path,
                        records: 0,
                        last_metrics: 0,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("No se pudo crear un nombre de registro único")
    }

    pub fn write(&mut self, mut event: Value) -> Result<()> {
        // Bound file size and always retain room for final/error events.
        if self.records >= 3600 {
            return Ok(());
        }
        let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
        if event["event"] == "metrics" {
            if self.records >= 3500
                || self.last_metrics != 0 && millis.saturating_sub(self.last_metrics) < 10_000
            {
                return Ok(());
            }
            self.last_metrics = millis;
        }
        event["unix_ms"] = json!(millis);
        serde_json::to_writer(&mut self.file, &event)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.records += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn log_is_private_bounded_and_does_not_follow_symlinks() {
        let path = std::env::temp_dir().join(format!("omarchy-log-test-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let sub = directory(&path, "safe").unwrap();
        assert_eq!(fs::metadata(&sub).unwrap().mode() & 0o777, 0o700);
        std::os::unix::fs::symlink(&sub, path.join("link")).unwrap();
        assert!(directory(&path, "link").is_err());
        fs::remove_file(path.join("link")).unwrap();
        fs::remove_dir(sub).unwrap();
        fs::remove_dir(path).unwrap();
    }
}

use std::fs::OpenOptions;
use std::io::Write;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;

use crate::state::MarketplacePaths;

pub struct MutationLock {
    path: Utf8PathBuf,
}

impl MutationLock {
    pub fn acquire(paths: &MarketplacePaths) -> Result<Self> {
        std::fs::create_dir_all(&paths.root)?;
        let path = paths.root.join("mutation.lock");
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|value| value.trim().parse::<u32>().ok())
                        .is_some_and(|pid| !process_is_running(pid));
                    if attempt == 0 && stale {
                        std::fs::remove_file(&path).with_context(|| {
                            format!("failed to clear stale marketplace mutation lock {path}")
                        })?;
                        continue;
                    }
                    bail!(
                        "another marketplace mutation is in progress; if no yasm process is running, remove {path}"
                    );
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("failed to acquire {path}"));
                }
            }
        }
        unreachable!("mutation lock acquisition attempts return or fail")
    }
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_is_running(_pid: u32) -> bool {
    true
}

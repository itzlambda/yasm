use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Context, Result};
use self_update::backends::github::Update;

use crate::interactive;

const GITHUB_OWNER: &str = "itzlambda";
const GITHUB_REPO: &str = "yasm";
const BIN_NAME: &str = "yasm";

pub fn run(yes: bool) -> Result<()> {
    if !yes && !std::io::stdin().is_terminal() {
        anyhow::bail!("missing confirmation; pass `--yes` to update without prompting");
    }

    let current = env!("CARGO_PKG_VERSION");
    let latest = Update::configure()
        .repo_owner(GITHUB_OWNER)
        .repo_name(GITHUB_REPO)
        .bin_name(BIN_NAME)
        .current_version(current)
        .show_output(false)
        .no_confirm(true)
        .timeout(Duration::from_secs(120))
        .build()
        .context("cannot configure update from GitHub releases")?
        .get_latest_release()
        .context("cannot check GitHub releases for itzlambda/yasm")?;
    if !latest
        .is_update_available()
        .context("cannot compare the current yasm version with the GitHub release")?
    {
        println!("yasm {current} is already the latest release");
        return Ok(());
    }
    let version = latest
        .latest()
        .context("cannot find a GitHub release for itzlambda/yasm")?
        .version()
        .to_string();

    if !yes {
        let confirmed = interactive::ask_confirm(
            "self-upgrade confirmation",
            &format!("Upgrade yasm from {current} to {version}?"),
            false,
            "pass `--yes` to update without prompting",
        )?;
        if !confirmed {
            println!("no changes made");
            return Ok(());
        }
    }

    let status = Update::configure()
        .repo_owner(GITHUB_OWNER)
        .repo_name(GITHUB_REPO)
        .bin_name(BIN_NAME)
        .current_version(current)
        .release_tag(format!("v{version}"))
        .show_download_progress(std::io::stderr().is_terminal())
        .unattended()
        .timeout(Duration::from_secs(120))
        .build()
        .context("cannot configure update from GitHub releases")?
        .update()
        .context("cannot replace the yasm binary from the GitHub release")?;
    if status.is_updated() {
        let installed = status.version();
        if installed != version {
            anyhow::bail!(
                "installed yasm {installed}, which is not the confirmed release {version}"
            );
        }
        println!("updated yasm to {installed}");
    } else {
        println!("yasm {} is already the latest release", status.version());
    }
    Ok(())
}

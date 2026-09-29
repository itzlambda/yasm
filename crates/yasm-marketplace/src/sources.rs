use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use sha2::{Digest, Sha256};
use yasm_core::{GitRef, SourceKind, SourceSpec};
use yasm_providers::fetch_source_cached;

use crate::catalogs::checked_relative;
use crate::model::{MarketplaceRecord, PluginSource};
use crate::state::MarketplacePaths;

pub struct FetchedRoot {
    pub root: Utf8PathBuf,
    pub revision: Option<String>,
}

pub fn fetch_marketplace(source: &SourceSpec, paths: &MarketplacePaths) -> Result<FetchedRoot> {
    let fetched = fetch_source_cached(source, &paths.cache.join("catalogs"))?;
    Ok(FetchedRoot {
        root: fetched.root,
        revision: fetched.resolved.map(|resolved| resolved.commit),
    })
}

pub fn fetch_plugin_source(
    marketplace: &MarketplaceRecord,
    source: &PluginSource,
    paths: &MarketplacePaths,
) -> Result<FetchedRoot> {
    match source {
        PluginSource::Relative { path } => {
            let fetched = fetch_marketplace(&marketplace.source, paths)?;
            Ok(FetchedRoot {
                root: checked_relative(&fetched.root, path)?,
                revision: fetched.revision,
            })
        }
        PluginSource::Git {
            url,
            path,
            r#ref,
            sha,
        } => {
            let checkout = fetch_git(url, r#ref.as_deref(), sha.as_deref(), paths)?;
            let root = match path {
                Some(path) => checked_relative(&checkout.root, path)?,
                None => checkout.root,
            };
            Ok(FetchedRoot {
                root,
                revision: checkout.revision,
            })
        }
    }
}

fn fetch_git(
    url: &str,
    requested_ref: Option<&str>,
    sha: Option<&str>,
    paths: &MarketplacePaths,
) -> Result<FetchedRoot> {
    if sha.is_none() {
        let spec = SourceSpec {
            kind: SourceKind::Github,
            path: url.to_string(),
            r#ref: requested_ref.map(GitRef::parse).transpose()?,
            subpath: None,
        };
        let fetched = fetch_source_cached(&spec, &paths.cache.join("plugins"))?;
        return Ok(FetchedRoot {
            root: fetched.root,
            revision: fetched.resolved.map(|resolved| resolved.commit),
        });
    }

    let sha = sha.context("commit pin is missing")?;
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid Git commit pin `{sha}`; expected a full 40-character commit SHA");
    }
    let key = format!("{:x}", Sha256::digest(format!("{url}\0{sha}").as_bytes()));
    let destination = paths.cache.join("plugins-pinned").join(key);
    match std::fs::symlink_metadata(&destination) {
        Ok(metadata)
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && destination.join(".git").is_dir() => {}
        Ok(_) => {
            bail!("plugin cache exists but is not a managed Git repository: {destination}");
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(destination.parent().context("cache path has no parent")?)?;
            run_git(
                Command::new("git")
                    .args(["clone", "--no-checkout", "--filter=blob:none", url])
                    .arg(destination.as_std_path()),
                "clone pinned plugin source",
            )?;
        }
        Err(error) => return Err(error.into()),
    }
    run_git(
        Command::new("git")
            .arg("-C")
            .arg(destination.as_std_path())
            .args(["fetch", "--depth", "1", "origin", sha]),
        "fetch pinned plugin commit",
    )?;
    run_git(
        Command::new("git")
            .arg("-C")
            .arg(destination.as_std_path())
            .args(["checkout", "--detach", "--force", sha]),
        "check out pinned plugin commit",
    )?;
    Ok(FetchedRoot {
        root: destination,
        revision: Some(sha.to_string()),
    })
}

fn run_git(command: &mut Command, action: &str) -> Result<()> {
    let output = command
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to run git to {action}"))?;
    if !output.status.success() {
        bail!(
            "failed to {action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn parse_source(input: &str) -> Result<SourceSpec> {
    input
        .parse::<yasm_providers::SourceInput>()?
        .into_spec()
        .map_err(Into::into)
}

//! Skill views contain only supported skill files, never native plugin registration files.
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

use crate::model::InstallationRecord;
use crate::state::MarketplacePaths;
use crate::store::digest_skill_export;

pub fn prepare_skill(
    installation: &InstallationRecord,
    relative: &str,
    paths: &MarketplacePaths,
) -> Result<Utf8PathBuf> {
    let source = installation.snapshot.join(relative);
    let parent = paths.root.join("outputs").join(installation.storage_key());
    std::fs::create_dir_all(&parent)?;
    let staging = tempfile::tempdir_in(&parent)?;
    let staging_path = Utf8Path::from_path(staging.path()).context("non-UTF-8 staging path")?;
    let root = std::fs::canonicalize(&installation.snapshot)?;
    for name in [
        "SKILL.md",
        "scripts",
        "references",
        "assets",
        "agents/openai.yaml",
    ] {
        let from = source.join(name);
        match std::fs::symlink_metadata(&from) {
            Ok(_) => copy_resource(&root, &from, &staging_path.join(name), &mut Vec::new())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if !staging_path.join("SKILL.md").is_file() {
        bail!("skill `{relative}` has no exportable SKILL.md");
    }
    let digest = digest_skill_export(staging_path)?;
    let destination = parent.join(&digest);
    if destination.exists() {
        if std::fs::symlink_metadata(&destination)?
            .file_type()
            .is_symlink()
            || digest_skill_export(&destination)? != digest
        {
            bail!("refusing to overwrite modified skill export {destination}");
        }
    } else {
        std::fs::rename(staging.path(), &destination)?;
    }
    Ok(destination)
}

fn copy_resource(
    package_root: &std::path::Path,
    source: &Utf8Path,
    destination: &Utf8Path,
    ancestors: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    // Materialize symlinks, so a resource cannot expose the original package via a link.
    let canonical = std::fs::canonicalize(source)?;
    if !canonical.starts_with(package_root) {
        bail!("skill resource resolves outside its package: {source}");
    }
    if ancestors.contains(&canonical) {
        bail!("cyclic skill resource: {source}");
    }
    let metadata = std::fs::metadata(&canonical)?;
    if metadata.is_dir() {
        ancestors.push(canonical.clone());
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("non-UTF-8 skill resource")?;
            // Never put auto-discoverable harness/plugin configuration in an exported view.
            if matches!(
                name,
                ".git"
                    | ".claude-plugin"
                    | ".codex-plugin"
                    | ".cursor-plugin"
                    | ".claude"
                    | ".cursor"
                    | ".agents"
                    | "plugin.json"
                    | ".mcp.json"
                    | "mcp.json"
                    | "hooks.json"
            ) {
                bail!("skill resource contains unsupported harness configuration: {name}");
            }
            copy_resource(
                package_root,
                &source.join(name),
                &destination.join(name),
                ancestors,
            )?;
        }
        ancestors.pop();
    } else if metadata.is_file() {
        std::fs::create_dir_all(destination.parent().context("resource has no parent")?)?;
        std::fs::copy(canonical, destination)?;
    } else {
        bail!("unsupported skill resource file type: {source}");
    }
    Ok(())
}

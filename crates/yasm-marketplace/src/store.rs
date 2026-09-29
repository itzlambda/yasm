use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};

use crate::state::MarketplacePaths;

pub fn snapshot_package(
    paths: &MarketplacePaths,
    plugin_id: &str,
    digest: &str,
    source: &Utf8Path,
) -> Result<Utf8PathBuf> {
    let destination = paths.packages().join(plugin_id).join(digest);
    let modified = match std::fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            if digest_package(&destination)? == digest {
                return Ok(destination);
            }
            true
        }
        Ok(_) => bail!("package snapshot path is not a managed directory: {destination}"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let parent = destination
        .parent()
        .context("package snapshot has no parent")?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".snapshot-")
        .tempdir_in(parent)?;
    let staging_root = Utf8Path::from_path(staging.path()).context("staging path is not UTF-8")?;
    copy_package(source, source, staging_root)?;
    // A modified cache entry may still belong to an installed receipt. Never reuse
    // or replace it: publish a fresh snapshot and let the transaction switch ownership.
    let destination = if modified {
        parent.join(format!(
            "{digest}-{}",
            staging_root
                .file_name()
                .context("staging path has no name")?
        ))
    } else {
        destination
    };
    std::fs::rename(staging_root, &destination)
        .with_context(|| format!("failed to publish {destination}"))?;
    Ok(destination)
}

fn copy_package(root: &Utf8Path, source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    let mut entries = std::fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if entry.file_name() == ".git" {
            continue;
        }
        let from = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
        let to = destination.join(entry.file_name().to_string_lossy().as_ref());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_package(root, &from, &to)?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to)?;
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&from)?;
            if target.is_absolute() {
                bail!("package symlink must be relative: {from}");
            }
            let resolved = Utf8PathBuf::from_path_buf(std::fs::canonicalize(&from)?)
                .map_err(|path| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
            let canonical_root = Utf8PathBuf::from_path_buf(std::fs::canonicalize(root)?)
                .map_err(|path| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
            if !resolved.starts_with(canonical_root) {
                bail!("package symlink resolves outside its source root: {from}");
            }
            copy_symlink(&from, &to)?;
        }
    }
    Ok(())
}

pub fn digest_package(root: &Utf8Path) -> Result<String> {
    digest_directory(root, false)
}

pub fn digest_skill_export(root: &Utf8Path) -> Result<String> {
    digest_directory(root, true)
}

fn digest_directory(root: &Utf8Path, include_modes: bool) -> Result<String> {
    fn visit(
        root: &Utf8Path,
        current: &Utf8Path,
        entries: &mut Vec<(String, Vec<u8>)>,
        include_modes: bool,
    ) -> Result<()> {
        let mut children = std::fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            if child.file_name() == ".git" {
                continue;
            }
            let path = Utf8PathBuf::from_path_buf(child.path())
                .map_err(|path| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
            let relative = path.strip_prefix(root)?.to_string();
            let kind = child.file_type()?;
            if kind.is_dir() {
                entries.push((format!("d:{relative}"), Vec::new()));
                visit(root, &path, entries, include_modes)?;
            } else if kind.is_file() {
                if include_modes {
                    let permissions = child.metadata()?.permissions();
                    #[cfg(unix)]
                    let mode = {
                        use std::os::unix::fs::PermissionsExt;
                        permissions.mode() & 0o777
                    };
                    #[cfg(not(unix))]
                    let mode = u32::from(permissions.readonly());
                    entries.push((format!("p:{relative}"), mode.to_le_bytes().to_vec()));
                }
                entries.push((format!("f:{relative}"), std::fs::read(path)?));
            } else if kind.is_symlink() {
                entries.push((
                    format!("l:{relative}"),
                    std::fs::read_link(path)?
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec(),
                ));
            }
        }
        Ok(())
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries, include_modes)?;
    let mut hasher = Sha256::new();
    for (path, bytes) in entries {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(unix)]
fn copy_symlink(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(source)?, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MarketplacePaths;

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinks_that_escape_the_package() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(root.join("secret"), "outside").unwrap();
        std::os::unix::fs::symlink("../secret", source.join("escape")).unwrap();
        let paths = MarketplacePaths::new(&root.join("data"), &root.join("cache"));

        let error = snapshot_package(&paths, "demo", "digest", &source).unwrap_err();
        assert!(error.to_string().contains("outside its source root"));
        assert!(!paths.packages().join("demo/digest").exists());
    }
}

#[cfg(windows)]
fn copy_symlink(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    let target = std::fs::read_link(source)?;
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)?;
    } else {
        std::os::windows::fs::symlink_file(target, destination)?;
    }
    Ok(())
}

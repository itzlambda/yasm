use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{io, Error, Result};

pub fn copy_dir_recursive(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    copy_dir_recursive_inner(source, destination, false)
}

pub fn copy_skill_source_dir_recursive(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    copy_dir_recursive_inner(source, destination, true)
}

fn copy_dir_recursive_inner(
    source: &Utf8Path,
    destination: &Utf8Path,
    exclude_git_metadata: bool,
) -> Result<()> {
    if !source.is_dir() {
        return Err(Error::Message(format!("{source} is not a directory")));
    }
    std::fs::create_dir_all(destination).map_err(|source| io(destination, source))?;

    for entry in std::fs::read_dir(source).map_err(|err| io(source, err))? {
        let entry = entry.map_err(|err| io(source, err))?;
        if exclude_git_metadata && entry.file_name() == ".git" {
            continue;
        }
        let from = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;
        let to = destination.join(entry.file_name().to_string_lossy().as_ref());
        let file_type = entry.file_type().map_err(|err| io(&from, err))?;
        if file_type.is_dir() {
            copy_dir_recursive_inner(&from, &to, exclude_git_metadata)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to).map_err(|err| io(&to, err))?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&from).map_err(|err| io(&from, err))?;
            create_symlink_path(&target, &to)?;
        }
    }
    let permissions = std::fs::metadata(source)
        .map_err(|err| io(source, err))?
        .permissions();
    std::fs::set_permissions(destination, permissions).map_err(|err| io(destination, err))?;
    Ok(())
}

pub fn ensure_skill_symlink(link: &Utf8Path, target: &Utf8Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|source| io(parent, source))?;
    }

    match std::fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let existing = std::fs::read_link(link).map_err(|source| io(link, source))?;
            let existing = Utf8PathBuf::from_path_buf(existing)
                .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;
            if existing == target {
                Ok(())
            } else {
                Err(Error::AgentSymlinkConflict {
                    link: link.to_path_buf(),
                    target: existing,
                })
            }
        }
        Ok(_) => Err(Error::AgentPathConflict(link.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_symlink(target, link),
        Err(source) => Err(io(link, source)),
    }
}

pub fn replace_directory_with_skill_symlink(link: &Utf8Path, target: &Utf8Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|source| io(parent, source))?;
    }

    match std::fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let existing = std::fs::read_link(link).map_err(|source| io(link, source))?;
            let existing = Utf8PathBuf::from_path_buf(existing)
                .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;
            Err(Error::AgentSymlinkConflict {
                link: link.to_path_buf(),
                target: existing,
            })
        }
        Ok(metadata) if metadata.is_dir() => {
            std::fs::remove_dir_all(link).map_err(|source| io(link, source))?;
            create_symlink(target, link)
        }
        Ok(_) => Err(Error::AgentPathConflict(link.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(Error::AgentPathConflict(link.to_path_buf()))
        }
        Err(source) => Err(io(link, source)),
    }
}

pub fn remove_symlink(link: &Utf8Path) -> Result<bool> {
    match std::fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(link).map_err(|source| io(link, source))?;
            Ok(true)
        }
        Ok(_) => Err(Error::AgentPathConflict(link.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io(link, source)),
    }
}

pub fn probe_symlink_support(parent: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(parent).map_err(|source| io(parent, source))?;
    let probe_target = parent.join(".yasm-symlink-probe-target");
    let probe_link = parent.join(".yasm-symlink-probe-link");
    let _ = std::fs::remove_file(&probe_link);
    let _ = std::fs::remove_dir_all(&probe_target);
    std::fs::create_dir_all(&probe_target).map_err(|source| io(&probe_target, source))?;
    let result = create_symlink(
        &Utf8PathBuf::from(".yasm-symlink-probe-target"),
        &probe_link,
    );
    let _ = std::fs::remove_file(&probe_link);
    let _ = std::fs::remove_dir_all(&probe_target);
    result.map_err(|error| Error::SymlinkUnsupported {
        path: parent.to_path_buf(),
        detail: format!("{error}; {}", symlink_remediation()),
    })
}

fn symlink_remediation() -> &'static str {
    if cfg!(windows) {
        "enable Windows Developer Mode or clone the repo inside WSL on the Linux filesystem, then retry"
    } else {
        "the filesystem does not support symbolic links"
    }
}

#[cfg(unix)]
fn create_symlink(target: &Utf8Path, link: &Utf8Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(|source| io(link, source))
}

#[cfg(unix)]
fn create_symlink_path(target: &std::path::Path, link: &Utf8Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(|source| io(link, source))
}

#[cfg(windows)]
fn create_symlink(target: &Utf8Path, link: &Utf8Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(target, link).map_err(|source| io(link, source))
}

#[cfg(windows)]
fn create_symlink_path(target: &std::path::Path, link: &Utf8Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(target, link).map_err(|source| io(link, source))
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn per_skill_symlink_is_idempotent_and_conflicts_with_dirs() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let target = root.join("store/frontend-design");
        let link = root.join("agent/frontend-design");
        std::fs::create_dir_all(&target).unwrap();

        ensure_skill_symlink(&link, &target).unwrap();
        ensure_skill_symlink(&link, &target).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target.as_std_path());

        let conflict = root.join("agent/conflict");
        std::fs::create_dir_all(&conflict).unwrap();
        assert!(matches!(
            ensure_skill_symlink(&conflict, &target),
            Err(Error::AgentPathConflict(_))
        ));
    }

    #[test]
    fn replace_directory_with_skill_symlink_replaces_real_directory() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let target = root.join("store/frontend-design");
        let link = root.join("agent/frontend-design");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&link).unwrap();
        std::fs::write(link.join("SKILL.md"), "old").unwrap();

        replace_directory_with_skill_symlink(&link, &target).unwrap();

        let metadata = std::fs::symlink_metadata(&link).unwrap();
        assert!(metadata.file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), target.as_std_path());
    }

    #[test]
    fn replace_directory_with_skill_symlink_rejects_real_file() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let target = root.join("store/frontend-design");
        let link = root.join("agent/frontend-design");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::write(&link, "old").unwrap();

        assert!(matches!(
            replace_directory_with_skill_symlink(&link, &target),
            Err(Error::AgentPathConflict(_))
        ));
        assert!(std::fs::symlink_metadata(&link).unwrap().is_file());
    }

    #[cfg(unix)]
    #[test]
    fn replace_directory_with_skill_symlink_rejects_wrong_target_symlink() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let target = root.join("store/frontend-design");
        let other = root.join("store/other");
        let link = root.join("agent/frontend-design");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();

        assert!(matches!(
            replace_directory_with_skill_symlink(&link, &target),
            Err(Error::AgentSymlinkConflict { .. })
        ));
        assert_eq!(std::fs::read_link(&link).unwrap(), other.as_std_path());
    }

    #[cfg(unix)]
    #[test]
    fn replace_directory_with_skill_symlink_rejects_broken_symlink() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let target = root.join("store/frontend-design");
        let missing = root.join("store/missing");
        let link = root.join("agent/frontend-design");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&missing, &link).unwrap();

        assert!(matches!(
            replace_directory_with_skill_symlink(&link, &target),
            Err(Error::AgentSymlinkConflict { .. })
        ));
        assert_eq!(std::fs::read_link(&link).unwrap(), missing.as_std_path());
    }
}

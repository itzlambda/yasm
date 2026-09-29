use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{io, Error, Result};
use crate::fs::{copy_dir_recursive, copy_skill_source_dir_recursive};
use crate::types::SkillId;

#[derive(Debug, Clone)]
pub struct Store {
    root: Utf8PathBuf,
}

impl Store {
    pub fn new(root: Utf8PathBuf) -> Self {
        Self { root }
    }

    pub fn skill_dir(&self, skill_id: &SkillId) -> Utf8PathBuf {
        self.root.join(skill_id.as_str())
    }

    pub fn install_skill_dir(
        &self,
        skill_id: &SkillId,
        source_dir: &Utf8Path,
    ) -> Result<Utf8PathBuf> {
        self.install_skill_dir_with(skill_id, source_dir, copy_dir_recursive)
    }

    pub fn install_skill_source_dir(
        &self,
        skill_id: &SkillId,
        source_dir: &Utf8Path,
    ) -> Result<Utf8PathBuf> {
        self.install_skill_dir_with(skill_id, source_dir, copy_skill_source_dir_recursive)
    }

    fn install_skill_dir_with(
        &self,
        skill_id: &SkillId,
        source_dir: &Utf8Path,
        copy: fn(&Utf8Path, &Utf8Path) -> Result<()>,
    ) -> Result<Utf8PathBuf> {
        std::fs::create_dir_all(&self.root).map_err(|source| io(&self.root, source))?;

        let destination = self.skill_dir(skill_id);
        let staging = tempfile::Builder::new()
            .prefix(".yasm-install-")
            .tempdir_in(&self.root)
            .map_err(|source| io(&self.root, source))?;
        let staging_root = utf8_temp_path(staging.path())?;
        let staged_skill = staging_root.join("skill");
        copy(source_dir, &staged_skill)?;

        if destination.exists() {
            replace_existing_directory(&self.root, &staged_skill, &destination)?;
        } else {
            std::fs::rename(&staged_skill, &destination)
                .map_err(|source| io(&destination, source))?;
        }

        Ok(destination)
    }

    pub fn remove_skill(&self, skill_id: &SkillId) -> Result<bool> {
        let path = self.skill_dir(skill_id);
        if path.exists() {
            std::fs::remove_dir_all(&path).map_err(|source| io(&path, source))?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

fn replace_existing_directory(
    root: &Utf8Path,
    staged: &Utf8Path,
    destination: &Utf8Path,
) -> Result<()> {
    if exchange_directories(staged, destination).is_ok() {
        return Ok(());
    }

    let backup = tempfile::Builder::new()
        .prefix(".yasm-backup-")
        .tempdir_in(root)
        .map_err(|source| io(root, source))?;
    let backup_root = utf8_temp_path(backup.path())?;
    let backup_skill = backup_root.join("skill");

    std::fs::rename(destination, &backup_skill).map_err(|source| io(destination, source))?;
    if let Err(install_error) = std::fs::rename(staged, destination) {
        if let Err(restore_error) = std::fs::rename(&backup_skill, destination) {
            let preserved_at = backup.keep();
            return Err(Error::Message(format!(
                "failed to replace {destination}: {install_error}; failed to restore the previous \
                 install: {restore_error}; previous content was preserved at {}",
                preserved_at.display()
            )));
        }
        return Err(io(destination, install_error));
    }

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exchange_directories(left: &Utf8Path, right: &Utf8Path) -> std::io::Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};

    renameat_with(
        CWD,
        left.as_std_path(),
        CWD,
        right.as_std_path(),
        RenameFlags::EXCHANGE,
    )
    .map_err(std::io::Error::from)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exchange_directories(_left: &Utf8Path, _right: &Utf8Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic directory exchange is not supported on this platform",
    ))
}

fn utf8_temp_path(path: &std::path::Path) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path.to_path_buf())
        .map_err(|path| Error::NonUtf8Path(path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn utf8(path: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path.to_path_buf()).unwrap()
    }

    fn write_skill(path: &Utf8Path, body: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn failed_staging_preserves_existing_install() {
        let temp = tempdir().unwrap();
        let root = utf8(temp.path()).join("skills");
        let source = utf8(temp.path()).join("source");
        let missing = utf8(temp.path()).join("missing");
        let store = Store::new(root.clone());
        let skill_id = SkillId::parse("demo").unwrap();
        write_skill(&source, "old");
        store.install_skill_dir(&skill_id, &source).unwrap();

        assert!(store.install_skill_dir(&skill_id, &missing).is_err());
        assert_eq!(
            std::fs::read_to_string(store.skill_dir(&skill_id).join("SKILL.md")).unwrap(),
            "old"
        );
        let entries = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from("demo")]);
    }

    // Apple filesystems reject invalid UTF-8 filenames before the copy can be exercised.
    #[cfg(all(unix, not(target_vendor = "apple")))]
    #[test]
    fn partially_copied_staging_tree_never_replaces_existing_install() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempdir().unwrap();
        let root = utf8(temp.path()).join("skills");
        let old_source = utf8(temp.path()).join("old-source");
        let invalid_source = utf8(temp.path()).join("invalid-source");
        let store = Store::new(root);
        let skill_id = SkillId::parse("demo").unwrap();
        write_skill(&old_source, "old");
        store.install_skill_dir(&skill_id, &old_source).unwrap();
        write_skill(&invalid_source, "candidate");
        std::fs::write(
            invalid_source
                .as_std_path()
                .join(std::ffi::OsString::from_vec(vec![0xff])),
            "invalid path",
        )
        .unwrap();

        assert!(store.install_skill_dir(&skill_id, &invalid_source).is_err());
        assert_eq!(
            std::fs::read_to_string(store.skill_dir(&skill_id).join("SKILL.md")).unwrap(),
            "old"
        );
    }

    #[test]
    fn staged_replacement_installs_complete_new_tree() {
        let temp = tempdir().unwrap();
        let root = utf8(temp.path()).join("skills");
        let old_source = utf8(temp.path()).join("old-source");
        let new_source = utf8(temp.path()).join("new-source");
        let store = Store::new(root.clone());
        let skill_id = SkillId::parse("demo").unwrap();
        write_skill(&old_source, "old");
        write_skill(&new_source, "new");
        std::fs::create_dir_all(new_source.join("nested")).unwrap();
        std::fs::write(new_source.join("nested/file.txt"), "complete").unwrap();
        store.install_skill_dir(&skill_id, &old_source).unwrap();

        store.install_skill_dir(&skill_id, &new_source).unwrap();

        let installed = store.skill_dir(&skill_id);
        assert_eq!(
            std::fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(installed.join("nested/file.txt")).unwrap(),
            "complete"
        );
        let entries = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from("demo")]);
    }

    #[test]
    fn install_excludes_git_checkout_metadata() {
        let temp = tempdir().unwrap();
        let root = utf8(temp.path()).join("skills");
        let source = utf8(temp.path()).join("source");
        let store = Store::new(root);
        let skill_id = SkillId::parse("demo").unwrap();
        write_skill(&source, "body");
        std::fs::create_dir_all(source.join(".git/objects")).unwrap();
        std::fs::write(source.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        store.install_skill_source_dir(&skill_id, &source).unwrap();

        let installed = store.skill_dir(&skill_id);
        assert!(installed.join("SKILL.md").is_file());
        assert!(!installed.join(".git").exists());
    }

    #[test]
    fn regular_install_preserves_git_directories() {
        let temp = tempdir().unwrap();
        let root = utf8(temp.path()).join("skills");
        let source = utf8(temp.path()).join("source");
        let store = Store::new(root);
        let skill_id = SkillId::parse("demo").unwrap();
        write_skill(&source, "body");
        std::fs::create_dir_all(source.join(".git")).unwrap();
        std::fs::write(source.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        store.install_skill_dir(&skill_id, &source).unwrap();

        assert_eq!(
            std::fs::read_to_string(store.skill_dir(&skill_id).join(".git/HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
    }
}

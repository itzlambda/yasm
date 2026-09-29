use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};

use crate::error::{io, Error, Result};

pub fn digest_skill_tree(root: &Utf8Path) -> Result<String> {
    digest_tree(root, false)
}

pub fn digest_skill_source_tree(root: &Utf8Path) -> Result<String> {
    digest_tree(root, true)
}

fn digest_tree(root: &Utf8Path, exclude_git_metadata: bool) -> Result<String> {
    let mut entries = Vec::new();
    entries.push((
        String::new(),
        b'd',
        permission_bytes(&std::fs::metadata(root).map_err(|source| io(root, source))?),
    ));
    collect_entries(root, root, &mut entries, exclude_git_metadata)?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    for (relative, kind, payload) in entries {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update([kind]);
        hasher.update((payload.len() as u64).to_le_bytes());
        hasher.update(&payload);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_entries(
    root: &Utf8Path,
    dir: &Utf8Path,
    entries: &mut Vec<(String, u8, Vec<u8>)>,
    exclude_git_metadata: bool,
) -> Result<()> {
    let mut children = std::fs::read_dir(dir)
        .map_err(|source| io(dir, source))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|source| io(dir, source))?;
    children.sort_by_key(|entry| entry.file_name());

    for entry in children {
        if exclude_git_metadata && entry.file_name() == ".git" {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| Error::Message(format!("{path} is outside {root}")))?
            .as_str()
            .replace('\\', "/");
        let file_type = entry.file_type().map_err(|source| io(&path, source))?;
        if file_type.is_dir() {
            let metadata = entry.metadata().map_err(|source| io(&path, source))?;
            entries.push((relative, b'd', permission_bytes(&metadata)));
            collect_entries(root, &path, entries, exclude_git_metadata)?;
        } else if file_type.is_file() {
            let metadata = entry.metadata().map_err(|source| io(&path, source))?;
            let mut bytes = permission_bytes(&metadata);
            bytes.extend(std::fs::read(&path).map_err(|source| io(&path, source))?);
            entries.push((relative, b'f', bytes));
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&path).map_err(|source| io(&path, source))?;
            entries.push((relative, b'l', target.to_string_lossy().as_bytes().to_vec()));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn permission_bytes(metadata: &std::fs::Metadata) -> Vec<u8> {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode().to_le_bytes().to_vec()
}

#[cfg(not(unix))]
fn permission_bytes(metadata: &std::fs::Metadata) -> Vec<u8> {
    vec![u8::from(metadata.permissions().readonly())]
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::tempdir;

    use super::{digest_skill_source_tree, digest_skill_tree};

    #[test]
    fn digest_is_stable_for_same_tree() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("SKILL.md"), "hello").unwrap();
        std::fs::write(root.join("nested/notes.md"), "world").unwrap();

        let first = digest_skill_tree(&root).unwrap();
        let second = digest_skill_tree(&root).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn digest_changes_when_file_content_changes() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("SKILL.md"), "hello").unwrap();
        let before = digest_skill_tree(&root).unwrap();
        std::fs::write(root.join("SKILL.md"), "hello!").unwrap();
        let after = digest_skill_tree(&root).unwrap();
        assert_ne!(before, after);
    }

    #[cfg(unix)]
    #[test]
    fn digest_changes_with_permissions_and_empty_directories() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("SKILL.md"), "hello").unwrap();
        let original = digest_skill_tree(&root).unwrap();
        std::fs::set_permissions(
            root.join("SKILL.md"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let executable = digest_skill_tree(&root).unwrap();
        std::fs::create_dir(root.join("empty")).unwrap();
        let with_empty_directory = digest_skill_tree(&root).unwrap();

        assert_ne!(original, executable);
        assert_ne!(executable, with_empty_directory);
    }

    #[test]
    fn source_digest_excludes_git_checkout_metadata_from_the_installable_tree() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("SKILL.md"), "hello").unwrap();
        let without_git = digest_skill_source_tree(&root).unwrap();

        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join(".git/objects/object"), "checkout metadata").unwrap();

        assert_eq!(digest_skill_source_tree(&root).unwrap(), without_git);
        assert_ne!(digest_skill_tree(&root).unwrap(), without_git);
    }
}

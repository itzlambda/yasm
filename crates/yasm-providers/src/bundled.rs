use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use yasm_core::{Error, Result};

pub const SELF_BUNDLE_ID: &str = "self";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundledSkill {
    pub id: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct BundledFile {
    path: &'static str,
    contents: &'static [u8],
}

const SELF_BUNDLE_SKILLS: &[BundledSkill] = &[BundledSkill { id: "yasm" }];

const SELF_BUNDLE_FILES: &[BundledFile] = &[BundledFile {
    path: "yasm/SKILL.md",
    contents: include_bytes!("../assets/self/yasm/SKILL.md"),
}];

pub fn self_bundle_skills() -> &'static [BundledSkill] {
    SELF_BUNDLE_SKILLS
}

pub fn self_bundle_digest() -> String {
    let mut hasher = Sha256::new();
    for skill in SELF_BUNDLE_SKILLS {
        hasher.update(skill.id.as_bytes());
        hasher.update([0]);
        hasher.update([0xff]);
    }
    for file in SELF_BUNDLE_FILES {
        hasher.update(file.path.as_bytes());
        hasher.update([0]);
        hasher.update((file.contents.len() as u64).to_le_bytes());
        hasher.update(file.contents);
    }
    format!("{:x}", hasher.finalize())
}

pub(crate) fn materialize_self_bundle(destination: &Utf8Path) -> Result<Utf8PathBuf> {
    if destination.exists() {
        return Ok(destination.to_path_buf());
    }
    let parent = destination.parent().ok_or_else(|| {
        Error::Message(format!(
            "embedded bundle destination has no parent: {destination}"
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(|source| yasm_core::error::io(parent, source))?;
    let staging = tempfile::Builder::new()
        .prefix(".yasm-bundle-")
        .tempdir_in(parent)
        .map_err(|source| yasm_core::error::io(parent, source))?;
    let staging_root = Utf8Path::from_path(staging.path()).ok_or_else(|| {
        Error::Message(format!(
            "embedded bundle staging path is not UTF-8: {}",
            staging.path().display()
        ))
    })?;

    for file in SELF_BUNDLE_FILES {
        let relative = Utf8Path::new(file.path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, camino::Utf8Component::ParentDir))
        {
            return Err(Error::Message(format!(
                "invalid embedded bundle path: {}",
                file.path
            )));
        }
        let path = staging_root.join(relative);
        let parent = path
            .parent()
            .ok_or_else(|| Error::Message(format!("embedded bundle path has no parent: {path}")))?;
        std::fs::create_dir_all(parent).map_err(|source| yasm_core::error::io(parent, source))?;
        std::fs::write(&path, file.contents)
            .map_err(|source| yasm_core::error::io(&path, source))?;
    }

    if let Err(source) = std::fs::rename(staging_root, destination) {
        if !destination.exists() {
            return Err(yasm_core::error::io(destination, source));
        }
    }

    Ok(destination.to_path_buf())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use yasm_core::discover_skills;

    use super::*;

    #[test]
    fn embedded_bundle_materializes_as_valid_skills() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().join("self")).unwrap();

        materialize_self_bundle(&root).unwrap();
        let skills = discover_skills(&root).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name.as_str(), "yasm");
        assert_eq!(skills[0].skill_path.as_str(), "yasm/SKILL.md");
        assert_eq!(self_bundle_digest().len(), 64);
    }

    #[test]
    fn concurrent_materialization_publishes_a_complete_bundle() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().join("self")).unwrap();
        let workers = (0..8)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || materialize_self_bundle(&root))
            })
            .collect::<Vec<_>>();

        for worker in workers {
            assert_eq!(worker.join().unwrap().unwrap(), root);
        }
        let skills = discover_skills(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name.as_str(), "yasm");
    }
}

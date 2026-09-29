use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use camino::Utf8Path;
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::error::{io, Error, Result};
use crate::types::{ResolvedSource, SkillId, SkillName, SkillPath, SourceSpec};

pub const LOCK_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(deserialize_with = "deserialize_lock_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bundles: BTreeMap<String, LockedBundleRecord>,
    pub skills: BTreeMap<SkillId, LockedSkillRecord>,
}

impl Default for LockFile {
    fn default() -> Self {
        Self {
            version: LOCK_VERSION,
            bundles: BTreeMap::new(),
            skills: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedSkillRecord {
    pub name: SkillName,
    pub source: SourceSpec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedSource>,
    pub skill_path: SkillPath,
    pub digest: String,
    pub enabled: BTreeSet<AgentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedBundleRecord {
    pub release: String,
    pub digest: String,
    pub members: BTreeSet<SkillId>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub excluded: BTreeSet<SkillId>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub enabled: BTreeSet<AgentId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub member_enabled: BTreeMap<SkillId, BTreeSet<AgentId>>,
}

impl LockFile {
    pub fn read(path: &Utf8Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path).map_err(|source| io(path, source))?;
        serde_json::from_str(&content).map_err(|source| Error::Json {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn write(&self, path: &Utf8Path) -> Result<()> {
        validate_lock_version(self.version)?;
        let content = serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            path: path.to_path_buf(),
            source,
        })?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_str().is_empty())
            .unwrap_or_else(|| Utf8Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| io(parent, source))?;

        let existing_permissions = std::fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions());
        let mut temporary = tempfile::Builder::new()
            .prefix(".yasm-lock-")
            .tempfile_in(parent)
            .map_err(|source| io(path, source))?;
        temporary
            .write_all(format!("{content}\n").as_bytes())
            .map_err(|source| io(path, source))?;
        temporary
            .flush()
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|source| io(path, source))?;
        if let Some(permissions) = existing_permissions {
            temporary
                .as_file()
                .set_permissions(permissions)
                .map_err(|source| io(path, source))?;
        }
        temporary
            .persist(path)
            .map_err(|error| io(path, error.error))?;
        sync_parent_directory(parent)
    }
}

fn validate_lock_version(version: u32) -> Result<()> {
    if version != LOCK_VERSION {
        return Err(Error::Message(format!(
            "unsupported lockfile version {version}; expected version {LOCK_VERSION}. Recreate Yasm state with the current executable"
        )));
    }
    Ok(())
}

fn deserialize_lock_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let version = u32::deserialize(deserializer)?;
    validate_lock_version(version).map_err(serde::de::Error::custom)?;
    Ok(version)
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Utf8Path) -> Result<()> {
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io(parent, source))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Utf8Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::tempdir;

    use super::*;
    use crate::types::{GitRef, SourceKind};

    fn sample_record(enabled: BTreeSet<AgentId>) -> LockedSkillRecord {
        LockedSkillRecord {
            name: SkillName::parse("frontend-design").unwrap(),
            source: SourceSpec {
                kind: SourceKind::Github,
                path: "https://github.com/owner/repo.git".to_string(),
                r#ref: Some(GitRef::parse("main").unwrap()),
                subpath: None,
            },
            resolved: Some(ResolvedSource {
                r#ref: Some(GitRef::parse("main").unwrap()),
                commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            }),
            skill_path: SkillPath::parse("skills/frontend-design/SKILL.md").unwrap(),
            digest: "abc123".to_string(),
            enabled,
        }
    }

    #[test]
    fn lockfile_roundtrips_v3_schema() {
        let temp = tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("yasm.lock")).unwrap();
        let mut lock = LockFile::default();
        lock.skills.insert(
            SkillId::parse("frontend-design").unwrap(),
            sample_record(BTreeSet::from([
                AgentId::parse("universal").unwrap(),
                AgentId::parse("claude").unwrap(),
            ])),
        );
        lock.bundles.insert(
            "self".to_string(),
            LockedBundleRecord {
                release: "0.1.0".to_string(),
                digest: "bundle-digest".to_string(),
                members: BTreeSet::from([SkillId::parse("frontend-design").unwrap()]),
                excluded: BTreeSet::new(),
                enabled: BTreeSet::from([AgentId::parse("claude").unwrap()]),
                member_enabled: BTreeMap::from([(
                    SkillId::parse("frontend-design").unwrap(),
                    BTreeSet::from([AgentId::parse("claude").unwrap()]),
                )]),
            },
        );

        lock.write(&path).unwrap();
        let read = LockFile::read(&path).unwrap();
        assert_eq!(read, lock);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\": 3"));
        assert!(raw.contains("\"enabled\""));
        assert!(!raw.contains("\"agents\""));
    }

    #[test]
    fn empty_enabled_set_roundtrips() {
        let temp = tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("yasm.lock")).unwrap();
        let mut lock = LockFile::default();
        lock.skills.insert(
            SkillId::parse("frontend-design").unwrap(),
            sample_record(BTreeSet::new()),
        );

        lock.write(&path).unwrap();
        let read = LockFile::read(&path).unwrap();
        assert_eq!(read, lock);
        assert!(read
            .skills
            .get(&SkillId::parse("frontend-design").unwrap())
            .unwrap()
            .enabled
            .is_empty());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"enabled\": []"));
    }

    #[cfg(unix)]
    #[test]
    fn write_atomically_replaces_existing_lockfile() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("yasm.lock")).unwrap();
        LockFile::default().write(&path).unwrap();
        let original_inode = std::fs::metadata(&path).unwrap().ino();

        let mut replacement = LockFile::default();
        replacement.skills.insert(
            SkillId::parse("frontend-design").unwrap(),
            sample_record(BTreeSet::new()),
        );
        replacement.write(&path).unwrap();

        assert_ne!(std::fs::metadata(&path).unwrap().ino(), original_inode);
        assert_eq!(LockFile::read(&path).unwrap(), replacement);
        let entries = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from("yasm.lock")]);
    }

    #[test]
    fn rejects_unsupported_versions_without_rewriting_the_lockfile() {
        let temp = tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("yasm.lock")).unwrap();
        for version in [0, 1, 2, LOCK_VERSION + 1, 999] {
            let raw = format!(r#"{{"version": {version}, "skills": {{}}}}"#);
            std::fs::write(&path, &raw).unwrap();
            let error = LockFile::read(&path).unwrap_err();
            assert!(error.to_string().contains("unsupported lockfile version"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
            // Recovery journals deserialize their embedded lockfile directly.
            assert!(serde_json::from_str::<LockFile>(&raw).is_err());
            let lock = LockFile {
                version,
                ..LockFile::default()
            };
            assert!(lock.write(&path).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        }
    }

    #[test]
    fn requires_current_skill_fields() {
        let mut value = serde_json::to_value(LockFile::default()).unwrap();
        value["skills"]["demo"] = serde_json::to_value(sample_record(BTreeSet::new())).unwrap();
        value["skills"]["demo"]
            .as_object_mut()
            .unwrap()
            .remove("enabled");
        let error = serde_json::from_value::<LockFile>(value).unwrap_err();
        assert!(error.to_string().contains("missing field `enabled`"));
    }

    #[test]
    fn rejects_invalid_newtypes_when_loading_lockfile() {
        let temp = tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("yasm.lock")).unwrap();
        let mut lock = LockFile::default();
        lock.skills.insert(
            SkillId::parse("demo").unwrap(),
            sample_record(BTreeSet::from([AgentId::parse("universal").unwrap()])),
        );
        lock.bundles.insert(
            "bundle".to_owned(),
            LockedBundleRecord {
                release: "1".to_owned(),
                digest: "digest".to_owned(),
                members: BTreeSet::from([SkillId::parse("demo").unwrap()]),
                excluded: BTreeSet::new(),
                enabled: BTreeSet::new(),
                member_enabled: BTreeMap::from([(
                    SkillId::parse("demo").unwrap(),
                    BTreeSet::from([AgentId::parse("universal").unwrap()]),
                )]),
            },
        );
        let valid = serde_json::to_value(&lock).unwrap();

        for (pointer, invalid, kind) in [
            ("/skills/demo/name", "a/b", "skill name"),
            ("/skills/demo/source/ref", "a..b", "git ref"),
            ("/skills/demo/resolved/ref", "-branch", "git ref"),
            ("/skills/demo/skill_path", "a/../b", "skill path"),
            ("/skills/demo/enabled/0", "a/b", "agent id"),
            ("/bundles/bundle/members/0", "..", "skill id"),
            ("/bundles/bundle/member_enabled/demo/0", "a/b", "agent id"),
        ] {
            let mut value = valid.clone();
            *value.pointer_mut(pointer).unwrap() = invalid.into();
            let raw = serde_json::to_string(&value).unwrap();
            std::fs::write(&path, &raw).unwrap();
            let error = LockFile::read(&path).unwrap_err();
            assert!(error.to_string().contains(kind), "{pointer}: {error}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        }

        for pointer in ["/skills", "/bundles/bundle/member_enabled"] {
            let mut value = valid.clone();
            let entries = value.pointer_mut(pointer).unwrap().as_object_mut().unwrap();
            let record = entries.remove("demo").unwrap();
            entries.insert("../bad".to_owned(), record);
            std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
            let error = LockFile::read(&path).unwrap_err();
            assert!(error.to_string().contains("skill id"), "{pointer}: {error}");
        }
    }
}

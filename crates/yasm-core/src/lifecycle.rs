use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};

use crate::agent::{Agent, AgentRegistry};
use crate::content::digest_skill_tree;
use crate::error::{Error, Result};
use crate::fs::{
    ensure_skill_symlink, probe_symlink_support, remove_symlink,
    replace_directory_with_skill_symlink,
};
use crate::lockfile::{LockFile, LockedSkillRecord};
use crate::paths::{normalize_path, YasmPaths};
use crate::store::Store;
use crate::types::{ResolvedSource, SkillId, SkillName, SkillPath, SourceKind, SourceSpec};

#[derive(Debug, Clone)]
pub enum LinkMode {
    Global,
    Project { root: Utf8PathBuf },
}

pub struct Lifecycle<'a> {
    pub store: &'a Store,
    pub registry: &'a AgentRegistry,
    pub paths: &'a YasmPaths,
    pub mode: LinkMode,
}

impl<'a> Lifecycle<'a> {
    pub fn skill_link_target(&self, agent: &Agent, skill_id: &SkillId) -> Result<Utf8PathBuf> {
        match &self.mode {
            LinkMode::Global => Ok(self.store.skill_dir(skill_id)),
            LinkMode::Project { root } => project_link_target(root, agent, skill_id),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn acquire(
        &self,
        lock: &mut LockFile,
        skill_id: &SkillId,
        name: SkillName,
        source: SourceSpec,
        resolved: Option<ResolvedSource>,
        skill_path: SkillPath,
        source_dir: &Utf8Path,
    ) -> Result<Utf8PathBuf> {
        let store_path = if source.kind == SourceKind::Github {
            self.store.install_skill_source_dir(skill_id, source_dir)?
        } else {
            self.store.install_skill_dir(skill_id, source_dir)?
        };
        let digest = digest_skill_tree(&store_path)?;
        let enabled = lock
            .skills
            .get(skill_id)
            .map(|record| record.enabled.clone())
            .unwrap_or_default();
        lock.skills.insert(
            skill_id.clone(),
            LockedSkillRecord {
                name,
                source,
                resolved,
                skill_path,
                digest,
                enabled,
            },
        );
        lock.write(&self.paths.lock_file())?;
        Ok(store_path)
    }

    pub fn enable(
        &self,
        lock: &mut LockFile,
        skill_id: &SkillId,
        agents: &[&Agent],
        replace: bool,
    ) -> Result<Vec<Utf8PathBuf>> {
        if !lock.skills.contains_key(skill_id) {
            return Err(Error::SkillNotAcquired(skill_id.to_string()));
        }
        let store_path = self.store.skill_dir(skill_id);
        if !store_path.exists() {
            return Err(Error::SkillNotAcquired(skill_id.to_string()));
        }

        let mut linked = Vec::new();
        let mut seen = BTreeSet::new();
        let mut probed_dirs = BTreeSet::new();
        for agent in agents {
            if probed_dirs.insert(agent.skill_dir.clone()) {
                probe_symlink_support(&agent.skill_dir)?;
            }
            let resolved_link = resolved_agent_skill_link(agent, skill_id)?;
            if !seen.insert(resolved_link.clone()) {
                if let Some(record) = lock.skills.get_mut(skill_id) {
                    record.enabled.insert(agent.id.clone());
                }
                linked.push(agent.skill_link(skill_id));
                continue;
            }
            let target = self.skill_link_target(agent, skill_id)?;
            let link = agent.skill_link(skill_id);
            match std::fs::symlink_metadata(&link) {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
                {
                    if replace {
                        replace_directory_with_skill_symlink(&link, &target)?;
                    } else {
                        return Err(Error::AgentPathConflict(link));
                    }
                }
                _ => ensure_skill_symlink(&link, &target)?,
            }
            if let Some(record) = lock.skills.get_mut(skill_id) {
                record.enabled.insert(agent.id.clone());
            }
            linked.push(link);
        }
        lock.write(&self.paths.lock_file())?;
        Ok(linked)
    }

    pub fn disable(
        &self,
        lock: &mut LockFile,
        skill_id: &SkillId,
        agents: &[&Agent],
    ) -> Result<Vec<Utf8PathBuf>> {
        if !lock.skills.contains_key(skill_id) {
            return Err(Error::SkillNotAcquired(skill_id.to_string()));
        }

        let mut removed = Vec::new();
        let mut seen = BTreeSet::new();
        for agent in agents {
            let resolved_link = resolved_agent_skill_link(agent, skill_id)?;
            if !seen.insert(resolved_link.clone()) {
                if let Some(record) = lock.skills.get_mut(skill_id) {
                    record.enabled.remove(&agent.id);
                }
                continue;
            }
            let link = agent.skill_link(skill_id);
            let expected = self.skill_link_target(agent, skill_id)?;
            match std::fs::symlink_metadata(&link) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let existing = std::fs::read_link(&link)
                        .map_err(|source| crate::error::io(&link, source))?;
                    let existing = Utf8PathBuf::from_path_buf(existing)
                        .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;
                    if existing == expected {
                        remove_symlink(&link)?;
                        removed.push(link);
                    } else {
                        return Err(Error::AgentSymlinkConflict {
                            link,
                            target: existing,
                        });
                    }
                }
                Ok(_) => return Err(Error::AgentPathConflict(link)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(crate::error::io(&link, source)),
            }
            if let Some(record) = lock.skills.get_mut(skill_id) {
                record.enabled.remove(&agent.id);
            }
        }
        lock.write(&self.paths.lock_file())?;
        Ok(removed)
    }

    pub fn remove(&self, lock: &mut LockFile, skill_id: &SkillId) -> Result<bool> {
        let Some(record) = lock.skills.get(skill_id) else {
            return Err(Error::SkillNotAcquired(skill_id.to_string()));
        };
        if !record.enabled.is_empty() {
            let agents = record
                .enabled
                .iter()
                .map(|id| id.as_str().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::SkillStillEnabled {
                skill: skill_id.to_string(),
                agents,
            });
        }
        let removed = self.store.remove_skill(skill_id)?;
        lock.skills.remove(skill_id);
        lock.write(&self.paths.lock_file())?;
        Ok(removed)
    }

    pub fn disable_all_then_remove(
        &self,
        lock: &mut LockFile,
        skill_id: &SkillId,
    ) -> Result<(Vec<Utf8PathBuf>, bool)> {
        let agents = {
            let Some(record) = lock.skills.get(skill_id) else {
                return Err(Error::SkillNotAcquired(skill_id.to_string()));
            };
            record
                .enabled
                .iter()
                .map(|id| self.registry.get(id.as_str()))
                .collect::<Result<Vec<_>>>()?
        };
        let links = self.disable(lock, skill_id, &agents)?;
        let removed = self.remove(lock, skill_id)?;
        Ok((links, removed))
    }
}

pub fn project_link_target(
    project_root: &Utf8Path,
    agent: &Agent,
    skill_id: &SkillId,
) -> Result<Utf8PathBuf> {
    let relative_agent_dir = agent
        .skill_dir
        .strip_prefix(project_root)
        .map_err(|_| Error::ProjectLinkEscape(agent.skill_dir.clone()))?;
    if relative_agent_dir
        .components()
        .any(|component| matches!(component, camino::Utf8Component::ParentDir))
    {
        return Err(Error::ProjectLinkEscape(agent.skill_dir.clone()));
    }
    let depth = relative_agent_dir.components().count();
    let mut target = Utf8PathBuf::new();
    for _ in 0..depth {
        target.push("..");
    }
    target.push(".yasm");
    target.push("skills");
    target.push(skill_id.as_str());
    if target.is_absolute() {
        return Err(Error::ProjectLinkEscape(target));
    }

    let absolute = normalize_path(&agent.skill_dir.join(&target));
    let skills_root = normalize_path(&project_root.join(".yasm/skills"));
    if !absolute.starts_with(&skills_root) {
        return Err(Error::ProjectLinkEscape(absolute));
    }
    Ok(target)
}

pub fn resolved_agent_skill_link(agent: &Agent, skill_id: &SkillId) -> Result<Utf8PathBuf> {
    let link = agent.skill_link(skill_id);
    let Some(parent) = agent.skill_dir.as_std_path().canonicalize().ok() else {
        return Ok(link);
    };
    let parent = Utf8PathBuf::from_path_buf(parent)
        .map_err(|path| Error::NonUtf8Path(path.display().to_string()))?;
    Ok(parent.join(skill_id.as_str()))
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::tempdir;

    use super::*;
    use crate::types::SourceKind;

    fn setup() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        YasmPaths,
        Store,
        AgentRegistry,
    ) {
        let data = tempdir().unwrap();
        let agents = tempdir().unwrap();
        let data_path = Utf8PathBuf::from_path_buf(data.path().to_path_buf()).unwrap();
        let agent_path = Utf8PathBuf::from_path_buf(agents.path().to_path_buf()).unwrap();
        let paths = YasmPaths {
            data_dir: data_path.clone(),
            cache_dir: data_path.join("cache"),
            config_dir: data_path.join("config"),
        };
        let store = Store::new(paths.skills_dir());
        let registry = AgentRegistry::with_home(agent_path);
        (data, agents, paths, store, registry)
    }

    fn write_skill(dir: &Utf8Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: demo\n---\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn acquire_leaves_enabled_empty() {
        let (_data, _agents, paths, store, registry) = setup();
        let source_dir = paths.cache_dir.join("src");
        write_skill(&source_dir, "body");
        let lifecycle = Lifecycle {
            store: &store,
            registry: &registry,
            paths: &paths,
            mode: LinkMode::Global,
        };
        let mut lock = LockFile::default();
        let skill_id = SkillId::parse("demo").unwrap();
        lifecycle
            .acquire(
                &mut lock,
                &skill_id,
                SkillName::parse("demo").unwrap(),
                SourceSpec {
                    kind: SourceKind::Local,
                    path: source_dir.to_string(),
                    r#ref: None,
                    subpath: None,
                },
                None,
                SkillPath::parse("SKILL.md").unwrap(),
                &source_dir,
            )
            .unwrap();

        let record = lock.skills.get(&skill_id).unwrap();
        assert!(record.enabled.is_empty());
        assert!(store.skill_dir(&skill_id).join("SKILL.md").exists());
        assert!(!record.digest.is_empty());
    }

    #[test]
    fn enable_and_disable_manage_links_and_activation_set() {
        let (_data, _agents, paths, store, registry) = setup();
        let source_dir = paths.cache_dir.join("src");
        write_skill(&source_dir, "body");
        let lifecycle = Lifecycle {
            store: &store,
            registry: &registry,
            paths: &paths,
            mode: LinkMode::Global,
        };
        let mut lock = LockFile::default();
        let skill_id = SkillId::parse("demo").unwrap();
        lifecycle
            .acquire(
                &mut lock,
                &skill_id,
                SkillName::parse("demo").unwrap(),
                SourceSpec {
                    kind: SourceKind::Local,
                    path: source_dir.to_string(),
                    r#ref: None,
                    subpath: None,
                },
                None,
                SkillPath::parse("SKILL.md").unwrap(),
                &source_dir,
            )
            .unwrap();

        let agent = registry.get("universal").unwrap();
        lifecycle
            .enable(&mut lock, &skill_id, &[agent], false)
            .unwrap();
        assert!(lock
            .skills
            .get(&skill_id)
            .unwrap()
            .enabled
            .contains(&agent.id));
        assert!(agent.skill_link(&skill_id).exists());

        lifecycle.disable(&mut lock, &skill_id, &[agent]).unwrap();
        assert!(lock.skills.get(&skill_id).unwrap().enabled.is_empty());
        assert!(!agent.skill_link(&skill_id).exists());
        assert!(store.skill_dir(&skill_id).exists());
    }

    #[test]
    fn remove_refuses_enabled_skill() {
        let (_data, _agents, paths, store, registry) = setup();
        let source_dir = paths.cache_dir.join("src");
        write_skill(&source_dir, "body");
        let lifecycle = Lifecycle {
            store: &store,
            registry: &registry,
            paths: &paths,
            mode: LinkMode::Global,
        };
        let mut lock = LockFile::default();
        let skill_id = SkillId::parse("demo").unwrap();
        lifecycle
            .acquire(
                &mut lock,
                &skill_id,
                SkillName::parse("demo").unwrap(),
                SourceSpec {
                    kind: SourceKind::Local,
                    path: source_dir.to_string(),
                    r#ref: None,
                    subpath: None,
                },
                None,
                SkillPath::parse("SKILL.md").unwrap(),
                &source_dir,
            )
            .unwrap();
        let agent = registry.get("universal").unwrap();
        lifecycle
            .enable(&mut lock, &skill_id, &[agent], false)
            .unwrap();

        assert!(matches!(
            lifecycle.remove(&mut lock, &skill_id),
            Err(Error::SkillStillEnabled { .. })
        ));
        lifecycle
            .disable_all_then_remove(&mut lock, &skill_id)
            .unwrap();
        assert!(!lock.skills.contains_key(&skill_id));
        assert!(!store.skill_dir(&skill_id).exists());
    }

    #[test]
    fn project_link_target_stays_inside_repo_store() {
        let root = Utf8PathBuf::from("/tmp/project");
        let registry = AgentRegistry::with_home(root.clone());
        let agent = registry.get("universal").unwrap();
        let skill_id = SkillId::parse("demo").unwrap();
        let target = project_link_target(&root, agent, &skill_id).unwrap();
        assert_eq!(target.as_str(), "../../.yasm/skills/demo");
        assert!(!target.is_absolute());
    }
}

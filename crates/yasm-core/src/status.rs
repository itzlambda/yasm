use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;

use crate::agent::{Agent, AgentRegistry};
use crate::content::digest_skill_tree;
use crate::error::Result;
use crate::lifecycle::Lifecycle;
use crate::lockfile::{LockFile, LockedSkillRecord};
use crate::paths::normalize_path;
use crate::store::Store;
use crate::types::SkillId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkHealth {
    Present,
    Missing,
    Foreign { target: Utf8PathBuf },
    Broken { target: Utf8PathBuf },
    PlainFile,
    UnmanagedDirectory,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentLinkStatus {
    pub agent: String,
    pub link: Utf8PathBuf,
    pub health: LinkHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inside_project_store: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillStatus {
    pub skill_id: SkillId,
    pub store_present: bool,
    pub digest_ok: Option<bool>,
    pub enabled: Vec<String>,
    pub links: Vec<AgentLinkStatus>,
}

pub fn collect_status(
    lifecycle: &Lifecycle<'_>,
    lock: &LockFile,
    store: &Store,
    registry: &AgentRegistry,
) -> Result<Vec<SkillStatus>> {
    let mut out = Vec::new();
    for (skill_id, record) in &lock.skills {
        out.push(skill_status(lifecycle, store, registry, skill_id, record)?);
    }
    Ok(out)
}

pub fn skill_status(
    lifecycle: &Lifecycle<'_>,
    store: &Store,
    registry: &AgentRegistry,
    skill_id: &SkillId,
    record: &LockedSkillRecord,
) -> Result<SkillStatus> {
    let store_path = store.skill_dir(skill_id);
    let store_present = store_path.is_dir();
    let digest_ok = if store_present {
        Some(digest_skill_tree(&store_path)? == record.digest)
    } else {
        None
    };

    let mut links = Vec::new();
    for agent in registry.all() {
        links.push(inspect_agent_link(lifecycle, agent, skill_id)?);
    }

    Ok(SkillStatus {
        skill_id: skill_id.clone(),
        store_present,
        digest_ok,
        enabled: record
            .enabled
            .iter()
            .map(|id| id.as_str().to_string())
            .collect(),
        links,
    })
}

pub fn inspect_agent_link(
    lifecycle: &Lifecycle<'_>,
    agent: &Agent,
    skill_id: &SkillId,
) -> Result<AgentLinkStatus> {
    let link = agent.skill_link(skill_id);
    let expected = lifecycle.skill_link_target(agent, skill_id)?;
    let health = classify_link(&link, &expected)?;
    let inside_project_store = match &lifecycle.mode {
        crate::lifecycle::LinkMode::Project { root } => {
            Some(project_link_resolves_inside_store(root, &link, &expected))
        }
        crate::lifecycle::LinkMode::Global => None,
    };
    Ok(AgentLinkStatus {
        agent: agent.id.as_str().to_string(),
        link,
        health,
        inside_project_store,
    })
}

pub fn classify_link(link: &Utf8Path, expected: &Utf8Path) -> Result<LinkHealth> {
    match std::fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target =
                std::fs::read_link(link).map_err(|source| crate::error::io(link, source))?;
            let target = Utf8PathBuf::from_path_buf(target)
                .map_err(|path| crate::error::Error::NonUtf8Path(path.display().to_string()))?;
            if target != expected {
                return Ok(LinkHealth::Foreign { target });
            }
            if link.exists() {
                Ok(LinkHealth::Present)
            } else {
                Ok(LinkHealth::Broken { target })
            }
        }
        Ok(metadata) if metadata.is_dir() => Ok(LinkHealth::UnmanagedDirectory),
        Ok(_) => Ok(LinkHealth::PlainFile),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LinkHealth::Missing),
        Err(source) => Err(crate::error::io(link, source)),
    }
}

fn project_link_resolves_inside_store(
    project_root: &Utf8Path,
    link: &Utf8Path,
    expected: &Utf8Path,
) -> bool {
    let Some(parent) = link.parent() else {
        return false;
    };
    let resolved = if expected.is_absolute() {
        expected.to_path_buf()
    } else {
        normalize_path(&parent.join(expected))
    };
    let skills_root = normalize_path(&project_root.join(".yasm/skills"));
    resolved.starts_with(&skills_root)
}

pub fn repair_owned_links(
    lifecycle: &Lifecycle<'_>,
    lock: &mut LockFile,
    replace: bool,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut repaired: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let skill_ids = lock.skills.keys().cloned().collect::<Vec<_>>();
    for skill_id in skill_ids {
        let enabled = lock
            .skills
            .get(&skill_id)
            .map(|record| record.enabled.clone())
            .unwrap_or_default();
        for agent_id in enabled {
            let agent = lifecycle.registry.get(agent_id.as_str())?;
            let expected = lifecycle.skill_link_target(agent, &skill_id)?;
            let link = agent.skill_link(&skill_id);
            match classify_link(&link, &expected)? {
                LinkHealth::Present | LinkHealth::Foreign { .. } => {}
                LinkHealth::Missing | LinkHealth::Broken { .. } => {
                    lifecycle.enable(lock, &skill_id, &[agent], replace)?;
                    repaired
                        .entry(skill_id.to_string())
                        .or_default()
                        .push(format!("recreated {}", agent.id.as_str()));
                }
                LinkHealth::PlainFile | LinkHealth::UnmanagedDirectory => {
                    if replace {
                        lifecycle.enable(lock, &skill_id, &[agent], true)?;
                        repaired
                            .entry(skill_id.to_string())
                            .or_default()
                            .push(format!("replaced {}", agent.id.as_str()));
                    }
                }
            }
        }
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::tempdir;

    use super::*;
    use crate::lifecycle::{Lifecycle, LinkMode};
    use crate::lockfile::LockFile;
    use crate::paths::YasmPaths;
    use crate::store::Store;
    use crate::types::{SkillName, SkillPath, SourceKind, SourceSpec};
    use crate::AgentRegistry;

    #[test]
    fn classify_link_detects_plain_file_and_missing() {
        let temp = tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let link = root.join("skill");
        let expected = root.join("store/skill");
        assert_eq!(
            classify_link(&link, &expected).unwrap(),
            LinkHealth::Missing
        );
        std::fs::write(&link, "not a link").unwrap();
        assert_eq!(
            classify_link(&link, &expected).unwrap(),
            LinkHealth::PlainFile
        );
    }

    #[test]
    fn status_reports_digest_mismatch_and_missing_link() {
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
        let lifecycle = Lifecycle {
            store: &store,
            registry: &registry,
            paths: &paths,
            mode: LinkMode::Global,
        };
        let source_dir = paths.cache_dir.join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("SKILL.md"), "---\nname: demo\n---\nbody\n").unwrap();
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

        let statuses = collect_status(&lifecycle, &lock, &store, &registry).unwrap();
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].store_present);
        assert_eq!(statuses[0].digest_ok, Some(true));
        assert!(statuses[0]
            .links
            .iter()
            .all(|link| link.health == LinkHealth::Missing));

        std::fs::write(store.skill_dir(&skill_id).join("SKILL.md"), "tampered").unwrap();
        let statuses = collect_status(&lifecycle, &lock, &store, &registry).unwrap();
        assert_eq!(statuses[0].digest_ok, Some(false));
    }
}

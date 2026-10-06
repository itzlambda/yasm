use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use yasm_core::{
    sanitize_skill_id, AgentId, DiscoveredSkill, LockFile, LockedBundleRecord, SkillId, SourceKind,
};
use yasm_providers::{self_bundle_digest, self_bundle_skills, SELF_BUNDLE_ID};

use crate::{acquired_self_member_ids, skill_directory_diff, ScopeContext};

/// Validated embedded skills, reused throughout one add or update operation.
pub(crate) struct SelfBundle<'a> {
    skills: BTreeMap<SkillId, &'a DiscoveredSkill>,
}

impl<'a> SelfBundle<'a> {
    pub(crate) fn from_skills(discovered: &'a [DiscoveredSkill]) -> Result<Self> {
        let skills = discovered
            .iter()
            .map(|skill| Ok((sanitize_skill_id(&skill.name)?, skill)))
            .collect::<yasm_core::Result<BTreeMap<_, _>>>()?;
        let declared = self_bundle_skills()
            .iter()
            .map(|skill| SkillId::parse(skill.id))
            .collect::<yasm_core::Result<BTreeSet<_>>>()?;
        let bundle = Self { skills };
        if bundle.members() != declared {
            anyhow::bail!("embedded self bundle manifest does not match its skill contents");
        }
        Ok(bundle)
    }

    pub(crate) fn skill(&self, skill_id: &SkillId) -> Option<&DiscoveredSkill> {
        self.skills.get(skill_id).copied()
    }

    pub(crate) fn members(&self) -> BTreeSet<SkillId> {
        self.skills.keys().cloned().collect()
    }

    pub(crate) fn matches_installed_contents(
        &self,
        context: &ScopeContext,
        installed: &BTreeSet<SkillId>,
    ) -> Result<bool> {
        for (skill_id, skill) in &self.skills {
            if installed.contains(skill_id)
                && skill_directory_diff(
                    skill_id,
                    &context.store.skill_dir(skill_id),
                    &skill.directory,
                )?
                .changed
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn is_converged(
        &self,
        context: &ScopeContext,
        lock: &LockFile,
        excluded: &BTreeSet<SkillId>,
    ) -> Result<bool> {
        for skill_id in self.skills.keys() {
            if excluded.contains(skill_id) {
                if lock.skills.contains_key(skill_id) {
                    return Ok(false);
                }
                continue;
            }
            let Some(record) = lock.skills.get(skill_id) else {
                return Ok(false);
            };
            if record.source.kind != SourceKind::Bundled || record.source.path != SELF_BUNDLE_ID {
                return Ok(false);
            }
        }
        let installed = acquired_self_member_ids(lock);
        if !installed
            .iter()
            .all(|skill_id| self.skills.contains_key(skill_id))
        {
            return Ok(false);
        }
        self.matches_installed_contents(context, &installed)
    }

    pub(crate) fn write_receipt(
        &self,
        context: &ScopeContext,
        lock: &mut LockFile,
        previous: Option<&LockedBundleRecord>,
        excluded: BTreeSet<SkillId>,
        enabled: BTreeSet<AgentId>,
    ) -> Result<()> {
        // Retain historical member selections, including members absent from this executable.
        let member_enabled = bundle_member_enabled(lock, previous);
        lock.bundles.insert(
            SELF_BUNDLE_ID.to_string(),
            LockedBundleRecord {
                release: env!("CARGO_PKG_VERSION").to_string(),
                digest: self_bundle_digest(),
                members: self.members(),
                excluded,
                enabled,
                member_enabled,
            },
        );
        lock.write(&context.paths.lock_file())?;
        Ok(())
    }
}

pub(crate) fn bundle_member_enabled(
    lock: &LockFile,
    previous: Option<&LockedBundleRecord>,
) -> BTreeMap<SkillId, BTreeSet<AgentId>> {
    let mut enabled = previous
        .map(|record| record.member_enabled.clone())
        .unwrap_or_default();
    for (skill_id, record) in &lock.skills {
        if record.source.kind == SourceKind::Bundled && record.source.path == SELF_BUNDLE_ID {
            enabled.insert(skill_id.clone(), record.enabled.clone());
        }
    }
    enabled
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use camino::Utf8PathBuf;
    use tempfile::TempDir;
    use yasm_core::{
        digest_skill_tree, discover_skills, AgentRegistry, LockFile, LockedSkillRecord, SkillId,
        Store, YasmPaths,
    };
    use yasm_providers::SourceInput;

    use crate::bundle::SelfBundle;
    use crate::{ResolvedScope, ScopeContext};

    struct Fixture {
        _temp: TempDir,
        context: ScopeContext,
        skills: Vec<yasm_core::DiscoveredSkill>,
        lock: LockFile,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            let paths = YasmPaths {
                data_dir: root.join("data"),
                cache_dir: root.join("cache"),
                config_dir: root.join("config"),
            };
            let context = ScopeContext {
                scope: ResolvedScope::Project { root: root.clone() },
                store: Store::new(paths.skills_dir()),
                registry: AgentRegistry::with_home(root.clone()),
                paths,
            };
            let source_root = root.join("source");
            let mut lock = LockFile::default();
            for name in ["first", "second"] {
                let contents =
                    format!("---\nname: {name}\ndescription: Test member\n---\n\n{name}\n");
                let skill_id = SkillId::parse(name).unwrap();
                for directory in [source_root.join(name), context.store.skill_dir(&skill_id)] {
                    std::fs::create_dir_all(&directory).unwrap();
                    std::fs::write(directory.join("SKILL.md"), &contents).unwrap();
                }
            }
            let skills = discover_skills(&source_root).unwrap();
            for skill in &skills {
                lock.skills.insert(
                    SkillId::parse(skill.name.as_str()).unwrap(),
                    LockedSkillRecord {
                        name: skill.name.clone(),
                        source: SourceInput::Bundled.into_spec().unwrap(),
                        resolved: None,
                        skill_path: skill.skill_path.clone(),
                        digest: digest_skill_tree(&skill.directory).unwrap(),
                        enabled: BTreeSet::new(),
                    },
                );
            }
            Self {
                _temp: temp,
                context,
                skills,
                lock,
            }
        }

        fn bundle(&self) -> SelfBundle<'_> {
            // Synthetic membership exercises convergence independently of the singleton manifest.
            SelfBundle {
                skills: self
                    .skills
                    .iter()
                    .map(|skill| (SkillId::parse(skill.name.as_str()).unwrap(), skill))
                    .collect(),
            }
        }
    }

    #[test]
    fn convergence_requires_every_member_to_be_installed_and_match() {
        let mut fixture = Fixture::new();
        let excluded = BTreeSet::new();
        assert!(fixture
            .bundle()
            .is_converged(&fixture.context, &fixture.lock, &excluded)
            .unwrap());

        let second = SkillId::parse("second").unwrap();
        std::fs::write(
            fixture.context.store.skill_dir(&second).join("SKILL.md"),
            "edited\n",
        )
        .unwrap();
        assert!(!fixture
            .bundle()
            .is_converged(&fixture.context, &fixture.lock, &excluded)
            .unwrap());

        fixture.lock.skills.remove(&second);
        std::fs::remove_dir_all(fixture.context.store.skill_dir(&second)).unwrap();
        assert!(!fixture
            .bundle()
            .is_converged(&fixture.context, &fixture.lock, &excluded)
            .unwrap());
        // Add can compare a matching installed subset without claiming full convergence.
        assert!(fixture
            .bundle()
            .matches_installed_contents(
                &fixture.context,
                &fixture.lock.skills.keys().cloned().collect()
            )
            .unwrap());
    }

    #[test]
    fn excluded_members_must_be_absent_for_convergence() {
        let mut fixture = Fixture::new();
        let second = SkillId::parse("second").unwrap();
        let excluded = BTreeSet::from([second.clone()]);
        assert!(!fixture
            .bundle()
            .is_converged(&fixture.context, &fixture.lock, &excluded)
            .unwrap());

        fixture.lock.skills.remove(&second);
        std::fs::remove_dir_all(fixture.context.store.skill_dir(&second)).unwrap();
        assert!(fixture
            .bundle()
            .is_converged(&fixture.context, &fixture.lock, &excluded)
            .unwrap());
    }
}

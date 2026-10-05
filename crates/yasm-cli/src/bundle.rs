use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use yasm_core::{
    sanitize_skill_id, AgentId, DiscoveredSkill, LockFile, LockedBundleRecord, SkillId, SourceKind,
};
use yasm_providers::{self_bundle_digest, self_bundle_skills, SELF_BUNDLE_ID};

use crate::{acquired_self_member_ids, skill_directory_diff, ScopeContext};

/// Validated embedded skills, reused throughout one add or update operation.
pub(crate) struct SelfBundle<'a> {
    pub(crate) skills: BTreeMap<SkillId, &'a DiscoveredSkill>,
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

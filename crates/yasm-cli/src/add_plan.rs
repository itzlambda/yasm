use anyhow::Result;
use yasm_core::{
    sanitize_skill_id, DiscoveredSkill, GitRef, LockFile, ResolvedSource, SkillId, SkillPath,
    SourceKind, SourceSpec,
};

/// Upstream identity is independent of the local installation name and commit.
#[derive(Debug)]
struct UpstreamSkill {
    kind: SourceKind,
    repository: String,
    requested_ref: Option<GitRef>,
    git_ref: Option<GitRef>,
    path: SkillPath,
}

impl UpstreamSkill {
    fn matches(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.repository == other.repository
            && self.path == other.path
            && (self.requested_ref == other.requested_ref || self.git_ref == other.git_ref)
    }

    fn new(
        source: &SourceSpec,
        resolved: Option<&ResolvedSource>,
        skill_path: &SkillPath,
    ) -> Result<Self> {
        Ok(Self {
            kind: source.kind.clone(),
            repository: source.path.clone(),
            requested_ref: source.r#ref.clone(),
            git_ref: source
                .r#ref
                .clone()
                .or_else(|| resolved.and_then(|resolved| resolved.r#ref.clone())),
            path: repository_skill_path(source, skill_path)?,
        })
    }
}

pub fn repository_skill_path(source: &SourceSpec, skill_path: &SkillPath) -> Result<SkillPath> {
    match &source.subpath {
        Some(subpath) => Ok(SkillPath::parse(format!(
            "{}/{}",
            subpath.as_str(),
            skill_path.as_str()
        ))?),
        None => Ok(skill_path.clone()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSkillState {
    New,
    Installed,
    Conflict,
}

pub struct PlannedSkill<'a> {
    pub skill_id: SkillId,
    pub skill: &'a DiscoveredSkill,
    pub state: AddSkillState,
}

/// Both explicit selection and the TTY picker consume this classification.
pub fn plan_skills<'a>(
    discovered: &'a [DiscoveredSkill],
    source: &SourceSpec,
    resolved: Option<&ResolvedSource>,
    lock: &LockFile,
) -> Result<Vec<PlannedSkill<'a>>> {
    discovered
        .iter()
        .map(|skill| {
            let skill_id = sanitize_skill_id(&skill.name)?;
            let state = match lock.skills.get(&skill_id) {
                None => AddSkillState::New,
                Some(record) => {
                    let candidate = UpstreamSkill::new(source, resolved, &skill.skill_path)?;
                    let installed = UpstreamSkill::new(
                        &record.source,
                        record.resolved.as_ref(),
                        &record.skill_path,
                    )?;
                    if candidate.matches(&installed) {
                        AddSkillState::Installed
                    } else {
                        AddSkillState::Conflict
                    }
                }
            };
            Ok(PlannedSkill {
                skill_id,
                skill,
                state,
            })
        })
        .collect()
}

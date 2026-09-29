pub mod agent;
pub mod content;
pub mod discovery;
pub mod error;
pub mod fs;
pub mod lifecycle;
pub mod lockfile;
pub mod metadata;
pub mod paths;
pub mod status;
pub mod store;
pub mod types;

pub use agent::{Agent, AgentId, AgentRegistry, BuiltInAgent, LinkTarget};
pub use content::{digest_skill_source_tree, digest_skill_tree};
pub use discovery::{
    discover_skills, discover_skills_with_diagnostics, parse_skill_file, DiscoveredSkill,
    SkillDiscovery, SkippedSkill,
};
pub use error::{Error, Result};
pub use fs::ensure_skill_symlink;
pub use lifecycle::{resolved_agent_skill_link, Lifecycle, LinkMode};
pub use lockfile::{LockFile, LockedBundleRecord, LockedSkillRecord};
pub use metadata::{
    read_skill_metadata, Harness, InvocationMetadata, InvocationStatus, MetadataDiagnostic,
    SkillMetadata,
};
pub use paths::{find_project_root, YasmPaths};
pub use status::{collect_status, repair_owned_links, LinkHealth};
pub use store::Store;
pub use types::{
    sanitize_skill_id, GitRef, ResolvedSource, SkillId, SkillName, SkillPath, SourceKind,
    SourceSpec,
};

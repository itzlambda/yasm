mod add_plan;
mod agent_files;
mod binary_update;
mod bundle;
mod terminal_diff;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::builder::{PossibleValuesParser, TypedValueParser};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::json;
use similar::TextDiff;
use tabled::{
    settings::{style::HorizontalLine, Style},
    Table, Tabled,
};
use tempfile::tempdir;
use yasm_core::{
    collect_status, digest_skill_source_tree, digest_skill_tree, discover_skills_with_diagnostics,
    ensure_skill_symlink, find_project_root, parse_skill_file, read_skill_metadata,
    repair_owned_links, sanitize_skill_id, Agent, AgentId, AgentRegistry, DiscoveredSkill, GitRef,
    Harness, InvocationStatus, Lifecycle, LinkHealth, LinkMode, LinkTarget, LockFile,
    LockedBundleRecord, LockedSkillRecord, SkillId, SkillMetadata, SkillPath, SkippedSkill,
    SourceKind, SourceSpec, Store, YasmPaths,
};
use yasm_providers::{
    fetch_checkout_cached, fetch_source, fetch_source_cached, resolve_fetched_source,
    self_bundle_digest, self_bundle_skills, FetchedSource, SourceInput, SELF_BUNDLE_ID,
};

mod interactive;
mod progress;

use self::bundle::{bundle_member_enabled, SelfBundle};

const DEFAULT_PAGER_COMMAND: &str = "less -R";
const DEFAULT_LESS_ENV: &str = "R";

#[derive(Debug, Parser)]
#[command(
    name = "yasm",
    about = "Manage AI agent skills",
    version,
    long_about = "Initialize projects, adopt existing skills, and manage skills for supported local agents.",
    after_help = "Examples:\n  yasm init --action apply\n  yasm init --no-migrate\n  yasm migrate --action review\n  yasm add anthropics/skills --skill frontend-design --action apply\n  yasm add ./skills --skill frontend-design --global --action apply\n  yasm list --enabled\n  yasm status\n  yasm self-upgrade --yes\n\nSupport: https://github.com/itzlambda/yasm",
    disable_version_flag = true
)]
struct Cli {
    #[arg(long, action = ArgAction::SetTrue, help = "Print version")]
    version: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, Args)]
struct ScopeArgs {
    #[arg(long, help = "Use the global yasm store")]
    global: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[cfg(feature = "marketplace")]
    #[command(
        about = "Manage plugin marketplace catalogs (experimental)",
        long_about = "Manage plugin marketplace catalogs (experimental).\n\nMarketplace and plugin support may change incompatibly or be removed in a future release."
    )]
    Marketplace(yasm_marketplace::cli::MarketplaceArgs),
    #[cfg(feature = "marketplace")]
    #[command(
        about = "Acquire and manage cross-agent plugins (experimental)",
        long_about = "Acquire and manage cross-agent plugins (experimental).\n\nMarketplace and plugin support may change incompatibly or be removed in a future release."
    )]
    Plugin(yasm_marketplace::cli::PluginArgs),
    #[command(about = "Initialize a Yasm project in the current directory")]
    Init {
        #[arg(long, value_enum, help = "Migration action to use without prompting")]
        action: Option<MigrationAction>,
        #[arg(
            long,
            conflicts_with_all = ["source", "no_migrate"],
            help = "Adopt only skills with a recorded or recommended upstream"
        )]
        with_upstream: bool,
        #[arg(
            long,
            conflicts_with = "no_migrate",
            help = "Adopt only this discovered skill; may be repeated"
        )]
        skill: Vec<String>,
        #[arg(
            long,
            conflicts_with_all = ["with_upstream", "no_migrate"],
            help = "Adopt selected skills as `local` or with this Git source"
        )]
        source: Option<String>,
        #[arg(
            long,
            conflicts_with = "action",
            help = "Initialize without adopting existing skills"
        )]
        no_migrate: bool,
    },
    #[command(about = "Adopt existing skills or agent files into the selected Yasm store")]
    Migrate {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(
            long,
            conflicts_with_all = ["skill", "with_upstream", "source"],
            help = "Adopt AGENTS.md and CLAUDE.md instead of skills"
        )]
        agent_files: bool,
        #[arg(long, help = "Adopt only this discovered skill; may be repeated")]
        skill: Vec<String>,
        #[arg(
            long,
            conflicts_with = "source",
            help = "Adopt only skills with a recorded or recommended upstream"
        )]
        with_upstream: bool,
        #[arg(
            long,
            conflicts_with = "with_upstream",
            help = "Adopt selected skills as `local` or with this Git source"
        )]
        source: Option<String>,
        #[arg(long, value_enum, help = "Migration action to use without prompting")]
        action: Option<MigrationAction>,
    },
    #[command(
        about = "Acquire skills into the yasm store and enable them for agents",
        after_help = "Examples:\n  yasm add self --global --action apply\n  yasm add ./skills --skill frontend-design --action apply\n  yasm add owner/repo --skill teach --agent claude --action apply\n  yasm add ./skills --skill frontend-design --no-enable --action apply"
    )]
    Add {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(
            value_parser = SourceValueParser,
            help = "`self`, local path, GitHub owner/repo shorthand, repository URL, SCP-style user@host:path, or /tree/main/<directory> URL"
        )]
        source: SourceInput,
        #[arg(
            long,
            help = "Skill name or repository-relative SKILL.md path to install, refresh, or repair"
        )]
        skill: Option<String>,
        #[arg(
            long,
            value_parser = agent_value_parser(),
            help = "Enable only these agents; omit to enable every built-in agent"
        )]
        agent: Vec<LinkTarget>,
        #[arg(
            long = "no-enable",
            conflicts_with = "agent",
            help = "Do not enable any agent; only fetch into the store"
        )]
        no_enable: bool,
        #[arg(long, value_enum, help = "Install action to use without prompting")]
        action: Option<ChangeAction>,
        #[arg(
            long,
            help = "Replace a different upstream source or unmanaged agent skill directories"
        )]
        replace: bool,
    },
    #[command(
        about = "Enable acquired skills for one or more agents",
        after_help = "Examples:\n  yasm enable self --global --agent claude\n  yasm enable frontend-design --agent claude\n  yasm enable frontend-design --agent universal --agent claude --replace"
    )]
    Enable {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(help = "Skill IDs or `self`; omit in TTY mode to choose interactively")]
        skills: Vec<String>,
        #[arg(long, value_parser = agent_value_parser(), help = "Agent target to link into")]
        agent: Vec<LinkTarget>,
        #[arg(
            long,
            help = "Replace existing unmanaged agent skill directories with managed links"
        )]
        replace: bool,
    },
    #[command(
        about = "Disable acquired skills for one or more agents without deleting the store copy",
        after_help = "Examples:\n  yasm disable self --global --agent claude\n  yasm disable frontend-design --agent claude"
    )]
    Disable {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(help = "Skill IDs or `self`; omit in TTY mode to choose interactively")]
        skills: Vec<String>,
        #[arg(long, value_parser = agent_value_parser(), help = "Agent target to unlink")]
        agent: Vec<LinkTarget>,
    },
    #[command(
        about = "List acquired and plugin-provided skills",
        long_about = "List acquired and plugin-provided skills. Inside a project, the default view includes both project and global skills; --global limits the output to global skills.",
        after_help = "Examples:\n  yasm list\n  yasm list --global\n  yasm list --enabled\n  yasm list --agent universal\n  yasm list --json"
    )]
    List {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(long, value_parser = agent_value_parser(), help = "Only show skills enabled for this agent")]
        agent: Option<LinkTarget>,
        #[arg(long, help = "Only show skills enabled for at least one agent")]
        enabled: bool,
        #[arg(long, help = "Print scoped skill lists as JSON")]
        json: bool,
    },
    #[command(
        about = "Show details for an acquired skill",
        after_help = "Examples:\n  yasm info self --global\n  yasm info frontend-design\n  yasm info frontend-design --global"
    )]
    Info {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(value_name = "skill-id", help = "Skill ID or `self` bundle to inspect")]
        skill: String,
    },
    #[command(
        about = "Show store and link health for acquired skills",
        after_help = "Examples:\n  yasm status\n  yasm status --json"
    )]
    Status {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(long, help = "Print status as JSON")]
        json: bool,
    },
    #[command(
        about = "Repair missing or dangling yasm-owned skill links",
        after_help = "Examples:\n  yasm doctor --repair\n  yasm doctor --repair --replace"
    )]
    Doctor {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(
            long,
            required = true,
            help = "Recreate missing yasm-owned links and remove dangling yasm-owned links"
        )]
        repair: bool,
        #[arg(
            long,
            requires = "repair",
            help = "Allow doctor --repair to replace unmanaged agent skill paths"
        )]
        replace: bool,
        #[arg(long, help = "Print a repair summary as JSON")]
        json: bool,
    },
    #[command(
        about = "Update acquired skills from their recorded sources",
        after_help = "Examples:\n  yasm update\n  yasm update self --global --action apply\n  yasm update frontend-design --action apply\n  yasm update frontend-design --action skip --json"
    )]
    Update {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(help = "Skill IDs or `self`; omit in TTY mode to choose interactively")]
        skills: Vec<String>,
        #[arg(long, value_enum, help = "Update action to use without prompting")]
        action: Option<ChangeAction>,
        #[arg(long, help = "Print an update summary as JSON")]
        json: bool,
    },
    #[command(
        about = "Remove acquired skills from the store",
        after_help = "Examples:\n  yasm remove self --global --all\n  yasm remove frontend-design\n  yasm disable frontend-design --agent claude\n  yasm remove frontend-design --all --json"
    )]
    Remove {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(help = "Skill IDs or `self`; omit in TTY mode to choose interactively")]
        skills: Vec<String>,
        #[arg(
            long,
            help = "Disable the skill for every agent, then delete the store copy"
        )]
        all: bool,
        #[arg(long, help = "Print a removal summary as JSON")]
        json: bool,
    },
    #[command(
        name = "self-upgrade",
        about = "Upgrade the yasm binary from GitHub releases",
        after_help = "Examples:\n  yasm self-upgrade\n  yasm self-upgrade --yes"
    )]
    SelfUpgrade {
        #[arg(long, help = "Replace the binary without prompting")]
        yes: bool,
    },
}

// Clap's default FromStr adapter echoes the rejected value, which may contain credentials.
#[derive(Clone)]
struct SourceValueParser;

impl TypedValueParser for SourceValueParser {
    type Value = SourceInput;

    fn parse_ref(
        &self,
        command: &clap::Command,
        _argument: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> std::result::Result<Self::Value, clap::Error> {
        let value = value.to_str().ok_or_else(|| {
            clap::Error::raw(
                clap::error::ErrorKind::InvalidUtf8,
                "source must be valid UTF-8",
            )
            .with_cmd(command)
        })?;
        value.parse().map_err(|error: yasm_core::Error| {
            clap::Error::raw(
                clap::error::ErrorKind::ValueValidation,
                format!("invalid source: {error}"),
            )
            .with_cmd(command)
        })
    }
}

fn agent_value_parser() -> impl TypedValueParser<Value = LinkTarget> {
    PossibleValuesParser::new(LinkTarget::ALL.iter().map(|agent| agent.as_str()))
        .try_map(|value| value.parse::<LinkTarget>())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ChangeAction {
    Review,
    Apply,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MigrationAction {
    Review,
    Apply,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.version {
        println!("yasm {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    match command {
        #[cfg(feature = "marketplace")]
        Command::Marketplace(args) => yasm_marketplace::cli::run_marketplace(args),
        #[cfg(feature = "marketplace")]
        Command::Plugin(args) => yasm_marketplace::cli::run_plugin(args),
        Command::Init {
            action,
            with_upstream,
            skill,
            source,
            no_migrate,
        } => init(action, &skill, with_upstream, source.as_deref(), no_migrate),
        Command::Migrate {
            scope,
            agent_files,
            skill,
            with_upstream,
            source,
            action,
        } => {
            let context = ScopeContext::resolve(scope)?;
            print_init_tip_if_relevant(&context, scope.global, false)?;
            if agent_files {
                agent_files::migrate(&context, action)
            } else {
                agent_files::require_no_pending(&context)?;
                migrate(&context, &skill, with_upstream, source.as_deref(), action)
            }
        }
        Command::Add {
            scope,
            source,
            skill,
            agent,
            no_enable,
            action,
            replace,
        } => {
            let context = ScopeContext::resolve(scope)?;
            context.require_no_pending_migration()?;
            print_init_tip_if_relevant(&context, scope.global, false)?;
            add(
                &context,
                source,
                skill.as_deref(),
                &agent,
                no_enable,
                action,
                replace,
            )
        }
        Command::Enable {
            scope,
            skills,
            agent,
            replace,
        } => {
            let context = ScopeContext::resolve(scope)?;
            context.require_no_pending_migration()?;
            print_init_tip_if_relevant(&context, scope.global, false)?;
            enable(&context, &skills, &agent, replace)
        }
        Command::Disable {
            scope,
            skills,
            agent,
        } => {
            let context = ScopeContext::resolve(scope)?;
            context.require_no_pending_migration()?;
            print_init_tip_if_relevant(&context, scope.global, false)?;
            disable(&context, &skills, &agent)
        }
        Command::List {
            scope,
            agent,
            enabled,
            json,
        } => {
            let contexts = ScopeContext::resolve_for_list(scope)?;
            list(&contexts, agent, enabled, json)
        }
        Command::Info { scope, skill } => {
            let context = ScopeContext::resolve(scope)?;
            info(&context, &skill)
        }
        Command::Status { scope, json } => {
            let context = ScopeContext::resolve(scope)?;
            print_init_tip_if_relevant(&context, scope.global, json)?;
            status(&context, json)
        }
        Command::Doctor {
            scope,
            repair,
            replace,
            json,
        } => {
            let context = ScopeContext::resolve(scope)?;
            context.require_no_pending_migration()?;
            print_init_tip_if_relevant(&context, scope.global, json)?;
            doctor(&context, repair, replace, json)
        }
        Command::Update {
            scope,
            skills,
            action,
            json,
        } => {
            let context = ScopeContext::resolve(scope)?;
            context.require_no_pending_migration()?;
            print_init_tip_if_relevant(&context, scope.global, json)?;
            update(&context, &skills, action, json)
        }
        Command::Remove {
            scope,
            skills,
            all,
            json,
        } => {
            let context = ScopeContext::resolve(scope)?;
            context.require_no_pending_migration()?;
            print_init_tip_if_relevant(&context, scope.global, json)?;
            remove(&context, &skills, all, json)
        }
        Command::SelfUpgrade { yes } => binary_update::run(yes),
    }
}

#[derive(Debug, Clone)]
enum ResolvedScope {
    Global,
    Project { root: Utf8PathBuf },
}

struct ScopeContext {
    scope: ResolvedScope,
    paths: YasmPaths,
    registry: AgentRegistry,
    store: Store,
}

impl ScopeContext {
    fn resolve(scope: ScopeArgs) -> Result<Self> {
        let cwd = current_dir_utf8()?;
        let resolved = if scope.global {
            ResolvedScope::Global
        } else if let Some(root) = find_project_root(&cwd) {
            ResolvedScope::Project { root }
        } else {
            ResolvedScope::Global
        };
        Self::from_scope(resolved)
    }

    fn resolve_for_list(scope: ScopeArgs) -> Result<Vec<Self>> {
        if scope.global {
            return Ok(vec![Self::from_scope(ResolvedScope::Global)?]);
        }

        let cwd = current_dir_utf8()?;
        let mut contexts = Vec::new();
        if let Some(root) = find_project_root(&cwd) {
            contexts.push(Self::from_scope(ResolvedScope::Project { root })?);
        }
        contexts.push(Self::from_scope(ResolvedScope::Global)?);
        Ok(contexts)
    }

    fn from_scope(scope: ResolvedScope) -> Result<Self> {
        let paths = match &scope {
            ResolvedScope::Global => YasmPaths::discover()?,
            ResolvedScope::Project { root } => YasmPaths::from_project_root(root.clone())?,
        };
        let registry = match &scope {
            ResolvedScope::Global => AgentRegistry::discover()?,
            ResolvedScope::Project { root } => AgentRegistry::with_home(root.clone()),
        };
        let store = Store::new(paths.skills_dir());

        Ok(Self {
            scope,
            paths,
            registry,
            store,
        })
    }

    fn lifecycle(&self) -> Lifecycle<'_> {
        Lifecycle {
            store: &self.store,
            registry: &self.registry,
            paths: &self.paths,
            mode: match &self.scope {
                ResolvedScope::Global => LinkMode::Global,
                ResolvedScope::Project { root } => LinkMode::Project { root: root.clone() },
            },
        }
    }

    fn list_title(&self) -> &'static str {
        match &self.scope {
            ResolvedScope::Global => "Global Skills",
            ResolvedScope::Project { .. } => "Project Skills",
        }
    }

    fn migration_root(&self) -> Result<Utf8PathBuf> {
        match &self.scope {
            ResolvedScope::Project { root } => Ok(root.clone()),
            ResolvedScope::Global => self
                .registry
                .all()
                .first()
                .and_then(|agent| agent.skill_dir.parent()?.parent())
                .map(Utf8Path::to_path_buf)
                .context("global agent registry has no migration root"),
        }
    }

    fn is_global(&self) -> bool {
        matches!(self.scope, ResolvedScope::Global)
    }

    fn is_project(&self) -> bool {
        matches!(self.scope, ResolvedScope::Project { .. })
    }

    fn require_no_pending_migration(&self) -> Result<()> {
        agent_files::require_no_pending(self)?;
        let journal = migration_journal_path(self);
        if std::fs::symlink_metadata(&journal).is_ok() {
            anyhow::bail!(
                "an interrupted migration must be recovered first; run `yasm migrate --action review`"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct MigrationCandidate {
    skill_id: SkillId,
    skill: DiscoveredSkill,
    digest: String,
    locations: Vec<(AgentId, Utf8PathBuf)>,
    canonical_locations: BTreeMap<Utf8PathBuf, Utf8PathBuf>,
    source: SourceSpec,
    source_reason: String,
    disposition: MigrationDisposition,
}

#[derive(Debug, Clone)]
enum MigrationDisposition {
    New,
    Adopt {
        record: Box<LockedSkillRecord>,
        store_digest: String,
        store_path: Utf8PathBuf,
    },
    Conflict {
        reason: String,
    },
}

#[derive(Debug)]
struct MigrationPlan {
    candidates: Vec<MigrationCandidate>,
    diagnostics: Vec<String>,
}

fn init(
    requested_action: Option<MigrationAction>,
    requested_skills: &[String],
    with_upstream: bool,
    requested_source: Option<&str>,
    no_migrate: bool,
) -> Result<()> {
    let cwd = current_dir_utf8()?;
    let home = std::env::var("HOME")
        .map(Utf8PathBuf::from)
        .context("could not discover home directory")?;
    let home = canonical_utf8(&home).unwrap_or(home);
    if canonical_utf8(&cwd).unwrap_or_else(|_| cwd.clone()) == home {
        anyhow::bail!("cannot initialize Yasm at the home directory; use `yasm migrate --global`");
    }

    let yasm_dir = cwd.join(".yasm");
    if std::fs::symlink_metadata(&yasm_dir).is_ok() {
        anyhow::bail!(
            "Yasm already exists in this directory. Run `yasm migrate` to adopt existing skills."
        );
    }
    let context = ScopeContext::from_scope(ResolvedScope::Project { root: cwd.clone() })?;

    if no_migrate {
        create_project_store(&context)?;
        println!("initialized project store at .yasm");
        return Ok(());
    }

    let mut plan = build_migration_plan(&context, requested_skills)?;
    enrich_migration_sources(&context, &mut plan.candidates)?;
    let interactive = requested_action.is_none() && std::io::stdin().is_terminal();
    if interactive {
        print_migration_diagnostics(&plan);
    } else {
        prepare_noninteractive_migration(
            &context,
            &mut plan.candidates,
            requested_skills,
            with_upstream,
            requested_source,
        )?;
        print_migration_plan(&context, &plan);
    }

    if plan.candidates.is_empty() {
        if requested_action == Some(MigrationAction::Review) {
            println!("review only; no changes made");
            return Ok(());
        }
        if requested_action.is_none() && !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "missing initialization action; pass `--action apply`, `--action review`, or `--no-migrate`"
            );
        }
        create_project_store(&context)?;
        println!("initialized project store at .yasm");
        return Ok(());
    }

    if interactive {
        run_interactive_migration(&context, plan.candidates, true)?;
        print_project_commit_hint(&context);
        return Ok(());
    }

    let action = requested_action.context(
        "missing initialization action; pass `--action apply`, `--action review`, or `--no-migrate`",
    )?;

    if action == MigrationAction::Review {
        println!("review only; no changes made");
        return Ok(());
    }
    apply_new_project_migration(&context, plan.candidates)?;
    print_project_commit_hint(&context);
    Ok(())
}

fn apply_new_project_migration(
    context: &ScopeContext,
    candidates: Vec<MigrationCandidate>,
) -> Result<()> {
    validate_migration_candidates(context, &candidates)?;
    create_project_store(context)?;
    apply_migration(context, candidates, true)?;
    Ok(())
}

fn create_project_store(context: &ScopeContext) -> Result<()> {
    let destination = &context.paths.data_dir;
    let parent = destination
        .parent()
        .context("project store has no parent")?;
    let staging = tempfile::Builder::new()
        .prefix(".yasm-init-")
        .tempdir_in(parent)
        .with_context(|| format!("failed to stage project store in {parent}"))?;
    let staging_path = Utf8PathBuf::from_path_buf(staging.path().to_path_buf())
        .map_err(|path| anyhow::anyhow!("non-UTF-8 staging path: {}", path.display()))?;
    std::fs::create_dir(staging_path.join("skills"))?;
    LockFile::default().write(&staging_path.join("yasm.lock"))?;
    let staging_path = Utf8PathBuf::from_path_buf(staging.keep())
        .map_err(|path| anyhow::anyhow!("non-UTF-8 staging path: {}", path.display()))?;
    if let Err(error) = rename_directory_noreplace(&staging_path, destination) {
        let _ = std::fs::remove_dir_all(&staging_path);
        return Err(error).with_context(|| format!("failed to create project store {destination}"));
    }
    sync_directory(parent)?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_directory_noreplace(source: &Utf8Path, destination: &Utf8Path) -> std::io::Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};

    renameat_with(
        CWD,
        source.as_std_path(),
        CWD,
        destination.as_std_path(),
        RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_directory_noreplace(source: &Utf8Path, destination: &Utf8Path) -> std::io::Result<()> {
    let _ = (source, destination);
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic project initialization is unsupported on this platform",
    ))
}

fn migrate(
    context: &ScopeContext,
    requested_skills: &[String],
    with_upstream: bool,
    requested_source: Option<&str>,
    requested_action: Option<MigrationAction>,
) -> Result<()> {
    recover_interrupted_migration(context)?;
    let mut plan = build_migration_plan(context, requested_skills)?;
    enrich_migration_sources(context, &mut plan.candidates)?;
    if plan.candidates.is_empty() {
        print_migration_plan(context, &plan);
        if context.is_global() && requested_action == Some(MigrationAction::Apply) {
            ensure_global_store(context)?;
        }
        if plan.diagnostics.is_empty() {
            println!("no unmanaged skills found");
        } else {
            println!("no migratable skills found; some paths were skipped (see warnings)");
        }
        return Ok(());
    }
    match requested_action {
        Some(MigrationAction::Review) => {
            prepare_noninteractive_migration(
                context,
                &mut plan.candidates,
                requested_skills,
                with_upstream,
                requested_source,
            )?;
            print_migration_plan(context, &plan);
            println!("review only; no changes made");
            return Ok(());
        }
        Some(MigrationAction::Apply) => {
            prepare_noninteractive_migration(
                context,
                &mut plan.candidates,
                requested_skills,
                with_upstream,
                requested_source,
            )?;
            print_migration_plan(context, &plan);
            apply_migration(context, plan.candidates, false)?;
        }
        None if std::io::stdin().is_terminal() => {
            print_migration_diagnostics(&plan);
            run_interactive_migration(context, plan.candidates, false)?;
        }
        None => {
            prepare_noninteractive_migration(
                context,
                &mut plan.candidates,
                requested_skills,
                with_upstream,
                requested_source,
            )?;
            print_migration_plan(context, &plan);
            anyhow::bail!("missing migration action; pass `--action apply` or `--action review`")
        }
    }
    print_project_commit_hint(context);
    Ok(())
}

fn ensure_global_store(context: &ScopeContext) -> Result<()> {
    std::fs::create_dir_all(context.paths.skills_dir())?;
    if !context.paths.lock_file().exists() {
        LockFile::default().write(&context.paths.lock_file())?;
    }
    Ok(())
}

fn print_migration_plan(context: &ScopeContext, plan: &MigrationPlan) {
    print_migration_diagnostics(plan);
    print_migration_scope(context);
    print_migration_candidates(&plan.candidates);
}

fn print_migration_scope(context: &ScopeContext) {
    let scope = match &context.scope {
        ResolvedScope::Project { .. } => "project",
        ResolvedScope::Global => "global",
    };
    println!(
        "Migration scope: {scope} · destination: {}",
        display_user_path(context.paths.data_dir.as_str())
    );
}

fn print_migration_diagnostics(plan: &MigrationPlan) {
    for diagnostic in &plan.diagnostics {
        eprintln!("warning: {diagnostic}");
    }
}

fn print_migration_candidates(candidates: &[MigrationCandidate]) {
    if !candidates.is_empty() {
        println!("Planned changes:");
        for candidate in candidates {
            let locations = candidate
                .locations
                .iter()
                .map(|(_, path)| display_user_path(path.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            match &candidate.disposition {
                MigrationDisposition::New => println!(
                    "  {}  migrate new: {} -> {} ({})",
                    candidate.skill_id,
                    locations,
                    migration_source_label(&candidate.source),
                    candidate.source_reason,
                ),
                MigrationDisposition::Adopt { store_path, .. } => println!(
                    "  {}  adopt existing: {} -> {} (reuse managed store)",
                    candidate.skill_id,
                    locations,
                    display_user_path(store_path.as_str()),
                ),
                MigrationDisposition::Conflict { reason } => {
                    println!("  {}  CONFLICT: {reason}", candidate.skill_id)
                }
            }
        }
    }
}

fn migration_source_label(source: &SourceSpec) -> String {
    if source.kind == SourceKind::Owned {
        return "owned".to_string();
    }
    let repository = source
        .path
        .strip_prefix("https://github.com/")
        .unwrap_or(&source.path)
        .trim_end_matches(".git");
    let prefix = if source.kind == SourceKind::Git {
        "git"
    } else {
        "github"
    };
    let mut label = format!("{prefix}:{repository}");
    if let Some(git_ref) = &source.r#ref {
        label.push('@');
        label.push_str(git_ref.as_str());
    }
    if let Some(subpath) = &source.subpath {
        label.push_str(" · ");
        label.push_str(subpath.as_str());
    }
    label
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillsCliLockEntry {
    source: Option<String>,
    source_url: Option<String>,
    source_type: Option<String>,
    skill_path: Option<String>,
    r#ref: Option<String>,
}

fn skills_cli_lock_path(context: &ScopeContext) -> Result<(Utf8PathBuf, &'static str, u64)> {
    match &context.scope {
        ResolvedScope::Project { root } => {
            Ok((root.join("skills-lock.json"), "skills-lock.json", 1))
        }
        ResolvedScope::Global => {
            if let Some(state) =
                std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty())
            {
                let state = Utf8PathBuf::from_path_buf(state.into())
                    .map_err(|path| anyhow::anyhow!("non-UTF-8 state path: {}", path.display()))?;
                return Ok((state.join("skills/.skill-lock.json"), ".skill-lock.json", 3));
            }
            let home = std::env::var("HOME").context("could not discover home directory")?;
            Ok((
                Utf8PathBuf::from(home).join(".agents/.skill-lock.json"),
                ".skill-lock.json",
                3,
            ))
        }
    }
}

fn read_skills_cli_lock(
    path: &Utf8Path,
    expected_version: u64,
) -> BTreeMap<String, SkillsCliLockEntry> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return BTreeMap::new();
    };
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(expected_version) {
        return BTreeMap::new();
    }
    value
        .get("skills")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(id, entry)| {
            serde_json::from_value(entry.clone())
                .ok()
                .map(|entry| (id.clone(), entry))
        })
        .collect()
}

fn normalize_skills_cli_path(path: &str) -> Option<Option<SkillPath>> {
    if path.starts_with('/') || path.contains('\\') || path.split('/').next()?.contains(':') {
        return None;
    }
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
        || parts.last() != Some(&"SKILL.md")
    {
        return None;
    }
    let directory = &parts[..parts.len() - 1];
    if directory.is_empty() {
        Some(None)
    } else {
        SkillPath::parse(directory.join("/")).ok().map(Some)
    }
}

fn source_from_skills_cli_entry(
    entry: &SkillsCliLockEntry,
    include_ref: bool,
) -> Option<SourceSpec> {
    if !matches!(entry.source_type.as_deref(), Some("github" | "git")) {
        return None;
    }
    let subpath = normalize_skills_cli_path(entry.skill_path.as_deref()?)?;
    let mut source = entry
        .source_url
        .iter()
        .chain(entry.source.iter())
        .find_map(|source| {
            let source = SourceInput::parse_remote(source).ok()?.into_spec().ok()?;
            source.kind.is_git().then_some(source)
        })?;
    source.subpath = subpath;
    source.r#ref = if include_ref {
        entry.r#ref.as_deref().map(GitRef::parse).transpose().ok()?
    } else {
        None
    };
    Some(source)
}

fn catalog_source(skill_id: &SkillId) -> Option<SourceSpec> {
    let (repository, subpath) = match skill_id.as_str() {
        "frontend-design" => ("anthropics/skills", "skills/frontend-design"),
        "find-skills" => ("vercel-labs/skills", "skills/find-skills"),
        "setup-matt-pocock-skills" => (
            "mattpocock/skills",
            "skills/engineering/setup-matt-pocock-skills",
        ),
        "ask-matt" => ("mattpocock/skills", "skills/engineering/ask-matt"),
        "git-guardrails-claude-code" => (
            "mattpocock/skills",
            "skills/misc/git-guardrails-claude-code",
        ),
        "migrate-to-shoehorn" => ("mattpocock/skills", "skills/misc/migrate-to-shoehorn"),
        "grill-me" => ("mattpocock/skills", "skills/productivity/grill-me"),
        "grill-with-docs" => ("mattpocock/skills", "skills/engineering/grill-with-docs"),
        "vercel-react-best-practices" => {
            ("vercel-labs/agent-skills", "skills/react-best-practices")
        }
        "vercel-composition-patterns" => {
            ("vercel-labs/agent-skills", "skills/composition-patterns")
        }
        "vercel-react-native-skills" => ("vercel-labs/agent-skills", "skills/react-native-skills"),
        "supabase-postgres-best-practices" => (
            "supabase/agent-skills",
            "skills/supabase-postgres-best-practices",
        ),
        "remotion-best-practices" => ("remotion-dev/skills", "skills/remotion-best-practices"),
        "hyperframes-cli" => ("heygen-com/hyperframes", "skills/hyperframes-cli"),
        "prisma-client-api" => ("prisma/skills", "prisma-client-api"),
        "emil-design-eng" => ("emilkowalski/skills", "skills/emil-design-eng"),
        "caveman" => ("JuliusBrussee/caveman", "skills/caveman"),
        "caveman-commit" => ("JuliusBrussee/caveman", "skills/caveman-commit"),
        "caveman-review" => ("JuliusBrussee/caveman", "skills/caveman-review"),
        "caveman-stats" => ("JuliusBrussee/caveman", "skills/caveman-stats"),
        "humanizer" => ("blader/humanizer", ""),
        "poteto-mode" => ("cursor/plugins", "pstack/skills/poteto-mode"),
        "setup-pstack" => ("cursor/plugins", "pstack/skills/setup-pstack"),
        _ => return None,
    };
    Some(SourceSpec {
        kind: SourceKind::Github,
        path: format!("https://github.com/{repository}.git"),
        r#ref: None,
        subpath: (!subpath.is_empty())
            .then(|| SkillPath::parse(subpath).expect("catalog paths are valid")),
    })
}

fn enrich_migration_sources(
    context: &ScopeContext,
    candidates: &mut [MigrationCandidate],
) -> Result<()> {
    let (lock_path, lock_name, expected_version) = skills_cli_lock_path(context)?;
    let skills_cli = read_skills_cli_lock(&lock_path, expected_version);
    for candidate in candidates {
        if !matches!(candidate.disposition, MigrationDisposition::New) {
            continue;
        }
        if let Some(source) = skills_cli
            .get(candidate.skill_id.as_str())
            .and_then(|entry| source_from_skills_cli_entry(entry, context.is_global()))
        {
            candidate.source = source;
            candidate.source_reason = format!("from install history ({lock_name})");
        } else if let Some(source) = catalog_source(&candidate.skill_id) {
            candidate.source = source;
            candidate.source_reason = "recommended".to_string();
        }
    }
    Ok(())
}

fn prepare_noninteractive_migration(
    context: &ScopeContext,
    candidates: &mut Vec<MigrationCandidate>,
    requested_skills: &[String],
    with_upstream: bool,
    requested_source: Option<&str>,
) -> Result<()> {
    if let Some(source) = requested_source {
        if requested_skills.is_empty() {
            anyhow::bail!("`--source` requires at least one `--skill <name>` argument");
        }
        if let Some(candidate) = candidates
            .iter()
            .find(|candidate| !matches!(candidate.disposition, MigrationDisposition::New))
        {
            anyhow::bail!(
                "cannot assign `--source` while reconciling `{}`; its managed state or conflict must be preserved",
                candidate.skill_id
            );
        }
        if source == "local" {
            for candidate in candidates {
                candidate.source = owned_migration_source();
                candidate.source_reason = "selected as local".to_string();
            }
        } else {
            if candidates.len() != 1 {
                anyhow::bail!(
                    "a GitHub `--source` requires exactly one matching `--skill`; available selected skills: {}",
                    candidates
                        .iter()
                        .map(|candidate| candidate.skill_id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let source = validate_manual_migration_source(context, &candidates[0], source)?;
            candidates[0].source = source;
            candidates[0].source_reason = "manually selected".to_string();
        }
    } else if with_upstream {
        candidates.retain(|candidate| candidate.source.kind != SourceKind::Owned);
    }
    Ok(())
}

fn owned_migration_source() -> SourceSpec {
    SourceSpec {
        kind: SourceKind::Owned,
        path: ".".to_string(),
        r#ref: None,
        subpath: None,
    }
}

fn validate_manual_migration_source(
    _context: &ScopeContext,
    candidate: &MigrationCandidate,
    input: &str,
) -> Result<SourceSpec> {
    let mut source = SourceInput::parse_remote(input)?.into_spec()?;
    let temp = tempdir()?;
    let progress = progress::Progress::start(
        format!("Checking source for {} ...", candidate.skill_id),
        false,
    );
    let fetched = fetch_source(&source, &utf8_temp_path(&temp)?)?;
    let discovery = discover_skills_with_diagnostics(&fetched.root)?;
    warn_skipped_skills(&progress, &discovery.skipped);
    progress.finish_and_clear();

    let mut matches = discovery
        .skills
        .iter()
        .filter_map(|skill| {
            sanitize_skill_id(&skill.name)
                .ok()
                .filter(|skill_id| skill_id == &candidate.skill_id)
                .map(|_| skill)
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.skill_path.as_str().cmp(right.skill_path.as_str()));
    let selected = match matches.as_slice() {
        [] => {
            let mut available = discovery
                .skills
                .iter()
                .filter_map(|skill| sanitize_skill_id(&skill.name).ok())
                .map(|skill_id| skill_id.to_string())
                .collect::<Vec<_>>();
            available.sort();
            available.dedup();
            anyhow::bail!(
                "skill `{}` was not found in the Git source; available skills: {}",
                candidate.skill_id,
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            );
        }
        [selected] => *selected,
        _ => anyhow::bail!(
            "skill `{}` is ambiguous in the Git source; matching paths: {}",
            candidate.skill_id,
            matches
                .iter()
                .map(|skill| skill.skill_path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    let relative_directory = Utf8Path::new(selected.skill_path.as_str())
        .parent()
        .filter(|path| !path.as_str().is_empty());
    source.subpath = match (&source.subpath, relative_directory) {
        (Some(base), Some(relative)) => Some(SkillPath::parse(
            Utf8Path::new(base.as_str()).join(relative).as_str(),
        )?),
        (None, Some(relative)) => Some(SkillPath::parse(relative.as_str())?),
        (existing, None) => existing.clone(),
    };
    Ok(source)
}

fn run_interactive_migration(
    context: &ScopeContext,
    mut remaining: Vec<MigrationCandidate>,
    initializing: bool,
) -> Result<()> {
    print_migration_scope(context);
    let adoption_count = remaining
        .iter()
        .filter(|candidate| matches!(candidate.disposition, MigrationDisposition::Adopt { .. }))
        .count();
    let upstream_count = remaining
        .iter()
        .filter(|candidate| {
            matches!(candidate.disposition, MigrationDisposition::New)
                && candidate.source.kind != SourceKind::Owned
        })
        .count();
    let source_decision_count = remaining
        .iter()
        .filter(|candidate| {
            matches!(candidate.disposition, MigrationDisposition::New)
                && candidate.source.kind == SourceKind::Owned
        })
        .count();
    let scope = match &context.scope {
        ResolvedScope::Project { .. } => "project",
        ResolvedScope::Global => "global",
    };
    println!(
        "Found {} unmanaged skill{} · {scope} scope",
        remaining.len(),
        if remaining.len() == 1 { "" } else { "s" }
    );
    println!(
        "{upstream_count} have an upstream · {source_decision_count} need a source decision · {adoption_count} can reconcile with an existing managed skill"
    );

    let mut initialized = !initializing;
    if adoption_count > 0 {
        let adoptions = remaining
            .iter()
            .filter(|candidate| matches!(candidate.disposition, MigrationDisposition::Adopt { .. }))
            .cloned()
            .collect::<Vec<_>>();
        let labels = adoptions
            .iter()
            .map(|candidate| {
                format!(
                    "{} -> existing managed store ({})",
                    candidate.skill_id,
                    migration_source_label(&candidate.source)
                )
            })
            .collect::<Vec<_>>();
        let selections = interactive::ask_multiselect(
            "managed skill reconciliation",
            "Select existing managed skills to reconcile (Enter reconciles selected)",
            &labels,
            &vec![true; labels.len()],
            "pass `--action apply` to reconcile managed copies without prompting",
        )?;
        let selected_indexes = selections.into_iter().collect::<BTreeSet<_>>();
        let selected_ids = adoptions
            .iter()
            .enumerate()
            .filter(|(index, _)| selected_indexes.contains(index))
            .map(|(_, candidate)| candidate.skill_id.clone())
            .collect::<BTreeSet<_>>();
        let selected = remaining
            .iter()
            .filter(|candidate| selected_ids.contains(&candidate.skill_id))
            .cloned()
            .collect::<Vec<_>>();
        remaining.retain(|candidate| !selected_ids.contains(&candidate.skill_id));
        if !selected.is_empty() {
            let count = selected.len();
            apply_migration_batch(context, selected, initializing, &mut initialized)?;
            println!(
                "Reconciled {count} existing managed skill{}.",
                if count == 1 { "" } else { "s" }
            );
        }
    }

    if upstream_count > 0 {
        let upstream = remaining
            .iter()
            .filter(|candidate| {
                matches!(candidate.disposition, MigrationDisposition::New)
                    && candidate.source.kind != SourceKind::Owned
            })
            .cloned()
            .collect::<Vec<_>>();
        let labels = upstream
            .iter()
            .map(|candidate| {
                format!(
                    "{} -> {}  {}",
                    candidate.skill_id,
                    migration_source_label(&candidate.source),
                    candidate.source_reason
                )
            })
            .collect::<Vec<_>>();
        let defaults = vec![true; labels.len()];
        let selections = interactive::ask_multiselect(
            "upstream migrations",
            "Select skills to migrate with these upstreams (Enter migrates selected)",
            &labels,
            &defaults,
            "pass `--with-upstream --action apply` to accept upstreams without prompting",
        )?;
        let selected_indexes = selections.into_iter().collect::<BTreeSet<_>>();
        let selected_ids = upstream
            .iter()
            .enumerate()
            .filter(|(index, _)| selected_indexes.contains(index))
            .map(|(_, candidate)| candidate.skill_id.clone())
            .collect::<BTreeSet<_>>();
        let selected = remaining
            .iter()
            .filter(|candidate| selected_ids.contains(&candidate.skill_id))
            .cloned()
            .collect::<Vec<_>>();
        remaining.retain(|candidate| !selected_ids.contains(&candidate.skill_id));
        if !selected.is_empty() {
            let count = selected.len();
            apply_migration_batch(context, selected, initializing, &mut initialized)?;
            println!(
                "Migrated {count} skill{} with upstream sources.",
                if count == 1 { "" } else { "s" }
            );
        }
    }

    while !remaining.is_empty() {
        println!(
            "\n{} skill{} remain:",
            remaining.len(),
            if remaining.len() == 1 { "" } else { "s" }
        );
        for candidate in &remaining {
            let reason = if matches!(candidate.disposition, MigrationDisposition::Conflict { .. }) {
                "migration conflict"
            } else if candidate.source.kind == SourceKind::Owned {
                "no upstream found"
            } else if candidate.source_reason == "recommended" {
                "suggested upstream declined"
            } else {
                "recorded upstream declined"
            };
            println!("  {}  {reason}", candidate.skill_id);
        }
        let has_source_decisions = remaining
            .iter()
            .any(|candidate| matches!(candidate.disposition, MigrationDisposition::New));
        let labels = if has_source_decisions {
            vec![
                "Keep some as local skills in Yasm".to_string(),
                "Set a Git source for a skill".to_string(),
                "Finish for now".to_string(),
            ]
        } else {
            vec!["Finish for now".to_string()]
        };
        let action = interactive::ask_select(
            "remaining skills action",
            "What would you like to do with the remaining skills?",
            &labels,
            "pass `--skill <name> --source <local|repository> --action apply`, or finish without another command",
        )?;
        if !has_source_decisions {
            break;
        }
        match action {
            0 => {
                let local_candidates = remaining
                    .iter()
                    .enumerate()
                    .filter(|(_, candidate)| {
                        matches!(candidate.disposition, MigrationDisposition::New)
                    })
                    .collect::<Vec<_>>();
                debug_assert!(!local_candidates.is_empty());
                let skill_labels = local_candidates
                    .iter()
                    .map(|(_, candidate)| candidate.skill_id.to_string())
                    .collect::<Vec<_>>();
                let selections = interactive::ask_multiselect(
                    "local skills",
                    "Select skills to keep as local",
                    &skill_labels,
                    &vec![false; skill_labels.len()],
                    "pass `--skill <name> --source local --action apply`",
                )?;
                let indexes = selections
                    .into_iter()
                    .map(|selection| local_candidates[selection].0)
                    .collect::<BTreeSet<_>>();
                let mut selected = Vec::new();
                let mut still_remaining = Vec::new();
                for (index, mut candidate) in remaining.into_iter().enumerate() {
                    if indexes.contains(&index) {
                        candidate.source = owned_migration_source();
                        candidate.source_reason = "selected as local".to_string();
                        selected.push(candidate);
                    } else {
                        still_remaining.push(candidate);
                    }
                }
                remaining = still_remaining;
                apply_migration_batch(context, selected, initializing, &mut initialized)?;
            }
            1 => {
                let sourceable = remaining
                    .iter()
                    .enumerate()
                    .filter(|(_, candidate)| {
                        matches!(candidate.disposition, MigrationDisposition::New)
                    })
                    .collect::<Vec<_>>();
                if sourceable.is_empty() {
                    println!(
                        "no remaining skills can accept a new source; finish for now and resolve managed-state conflicts separately"
                    );
                    continue;
                }
                let skill_labels = sourceable
                    .iter()
                    .map(|(_, candidate)| candidate.skill_id.to_string())
                    .collect::<Vec<_>>();
                let selected = interactive::ask_select(
                    "skill source",
                    "Choose a skill to give a Git source",
                    &skill_labels,
                    "pass `--skill <name> --source <repository> --action apply`",
                )?;
                let index = sourceable[selected].0;
                let input = interactive::ask_input(
                    "Git source",
                    "Git repository address or GitHub skill-directory URL",
                    "pass `--source <owner/repo>`, an SCP-style user@host:path address, or a GitHub tree URL",
                )?;
                let mut candidate = remaining[index].clone();
                candidate.source = validate_manual_migration_source(context, &candidate, &input)?;
                candidate.source_reason = "manually selected".to_string();
                apply_migration_batch(context, vec![candidate], initializing, &mut initialized)?;
                remaining.remove(index);
            }
            _ => break,
        }
    }

    if initializing && !initialized {
        create_project_store(context)?;
        println!("initialized project store at .yasm");
    }
    Ok(())
}

fn apply_migration_batch(
    context: &ScopeContext,
    candidates: Vec<MigrationCandidate>,
    initializing: bool,
    initialized: &mut bool,
) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    let initializing_batch = initializing && !*initialized;
    if initializing_batch {
        create_project_store(context)?;
        *initialized = true;
    }
    apply_migration(context, candidates, initializing_batch)
}

fn agent_directory_has_symlink_component(root: &Utf8Path, directory: &Utf8Path) -> Result<bool> {
    let relative = directory
        .strip_prefix(root)
        .with_context(|| format!("agent directory {directory} is outside migration root {root}"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {current}"))
            }
        }
    }
    Ok(false)
}

#[derive(Debug)]
struct DiscoveredMigration {
    skill_id: SkillId,
    copies: Vec<MigrationCopy>,
}

#[derive(Debug)]
struct MigrationCopy {
    skill: DiscoveredSkill,
    digest: String,
    canonical: Utf8PathBuf,
    locations: Vec<(AgentId, Utf8PathBuf)>,
}

fn build_migration_plan(context: &ScopeContext, requested: &[String]) -> Result<MigrationPlan> {
    let lock = LockFile::read(&context.paths.lock_file())?;
    let mut grouped: BTreeMap<SkillId, DiscoveredMigration> = BTreeMap::new();
    let mut aliases = Vec::new();
    let mut diagnostics = Vec::new();

    for agent in context.registry.all() {
        let migration_root = context.migration_root()?;
        if context.is_project()
            && agent_directory_has_symlink_component(&migration_root, &agent.skill_dir)?
        {
            diagnostics.push(format!(
                "skipped {}; agent skill directory contains a symlinked path component",
                agent.skill_dir
            ));
            continue;
        }
        match std::fs::metadata(&agent.skill_dir) {
            Ok(metadata) if !metadata.is_dir() => {
                diagnostics.push(format!(
                    "skipped {}; expected an agent skill directory",
                    agent.skill_dir
                ));
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if agent_directory_has_symlink_component(&migration_root, &agent.skill_dir)? {
                    diagnostics.push(format!(
                        "skipped {}; symlinked agent skill directory does not resolve to an existing directory",
                        agent.skill_dir
                    ));
                }
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", agent.skill_dir))
            }
            Ok(_) => {}
        }
        let entries = match std::fs::read_dir(&agent.skill_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", agent.skill_dir))
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("failed to read {}", agent.skill_dir))?;
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|path| anyhow::anyhow!("non-UTF-8 skill path: {}", path.display()))?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {path}"))?;
            if file_type.is_symlink() {
                let managed = path
                    .file_name()
                    .and_then(|name| SkillId::parse(name.to_string()).ok())
                    .and_then(|skill_id| {
                        let record = lock.skills.get(&skill_id)?;
                        if !record.enabled.contains(&agent.id) {
                            return None;
                        }
                        let expected = context
                            .lifecycle()
                            .skill_link_target(agent, &skill_id)
                            .ok()?;
                        let actual = std::fs::read_link(&path).ok()?;
                        (actual == expected.as_std_path()).then_some(())
                    })
                    .is_some();
                if managed {
                    continue;
                }
                let skill_id = path
                    .file_name()
                    .and_then(|name| SkillId::parse(name.to_string()).ok());
                let target = canonical_utf8(&path).ok();
                if let (Some(skill_id), Some(target)) = (skill_id, target) {
                    aliases.push((agent.id.clone(), path, skill_id, target));
                } else {
                    diagnostics.push(format!(
                        "skipped symlink {path}; it does not resolve to an adoptable skill"
                    ));
                }
                continue;
            }
            if !file_type.is_dir() {
                diagnostics.push(format!("skipped {path}; expected a skill directory"));
                continue;
            }
            let skill_file = path.join("SKILL.md");
            match std::fs::symlink_metadata(&skill_file) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    diagnostics.push(format!("skipped {path}; SKILL.md is a symlink"));
                    continue;
                }
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) | Err(_) => {
                    diagnostics.push(format!("skipped {path}; missing SKILL.md"));
                    continue;
                }
            }
            let skill = match parse_skill_file(&agent.skill_dir, &skill_file) {
                Ok(skill) => skill,
                Err(error) => {
                    diagnostics.push(format!("skipped {path}: {error}"));
                    continue;
                }
            };
            let skill_id = match sanitize_skill_id(&skill.name) {
                Ok(skill_id) => skill_id,
                Err(error) => {
                    diagnostics.push(format!("skipped {path}: {error}"));
                    continue;
                }
            };
            if skill_id.as_str() == SELF_BUNDLE_ID {
                diagnostics.push(format!(
                    "skipped {path}; skill ID `self` is reserved for Yasm bundled skills"
                ));
                continue;
            }
            if path.file_name() != Some(skill_id.as_str()) {
                diagnostics.push(format!(
                    "skipped {path}; directory name must match skill id `{skill_id}`"
                ));
                continue;
            }
            let digest = digest_skill_tree(&path)?;
            let canonical = canonical_utf8(&path)?;
            let discovered =
                grouped
                    .entry(skill_id.clone())
                    .or_insert_with(|| DiscoveredMigration {
                        skill_id,
                        copies: Vec::new(),
                    });
            if let Some(copy) = discovered
                .copies
                .iter_mut()
                .find(|copy| copy.canonical == canonical)
            {
                copy.locations.push((agent.id.clone(), path));
            } else {
                discovered.copies.push(MigrationCopy {
                    skill,
                    digest,
                    canonical,
                    locations: vec![(agent.id.clone(), path)],
                });
            }
        }
    }

    for (agent_id, path, skill_id, target) in aliases {
        let Some(discovered) = grouped.get_mut(&skill_id) else {
            diagnostics.push(format!(
                "skipped symlink {path}; it does not resolve to an adoptable skill"
            ));
            continue;
        };
        if let Some(copy) = discovered
            .copies
            .iter_mut()
            .find(|copy| copy.canonical == target)
        {
            copy.locations.push((agent_id, path));
        } else {
            diagnostics.push(format!(
                "skipped symlink {path}; it does not resolve to the canonical `{skill_id}` directory"
            ));
        }
    }

    let mut discovered = grouped.into_values().collect::<Vec<_>>();
    if !requested.is_empty() {
        let available = discovered
            .iter()
            .map(|skill| skill.skill_id.to_string())
            .collect::<Vec<_>>();
        let requested = requested.iter().collect::<BTreeSet<_>>();
        for skill in &requested {
            if !discovered.iter().any(|candidate| {
                candidate.skill_id.as_str() == skill.as_str()
                    || candidate
                        .copies
                        .iter()
                        .any(|copy| copy.skill.name.as_str() == skill.as_str())
            }) {
                anyhow::bail!(
                    "skill `{skill}` was not found; available unmanaged skills: {}",
                    if available.is_empty() {
                        "none".to_string()
                    } else {
                        available.join(", ")
                    }
                );
            }
        }
        discovered.retain(|candidate| {
            requested.iter().any(|skill| {
                candidate.skill_id.as_str() == skill.as_str()
                    || candidate
                        .copies
                        .iter()
                        .any(|copy| copy.skill.name.as_str() == skill.as_str())
            })
        });
    }
    let candidates = discovered
        .into_iter()
        .filter_map(|skill| classify_migration(context, &lock, skill).transpose())
        .collect::<Result<Vec<_>>>()?;
    Ok(MigrationPlan {
        candidates,
        diagnostics,
    })
}

fn classify_migration(
    context: &ScopeContext,
    lock: &LockFile,
    discovered: DiscoveredMigration,
) -> Result<Option<MigrationCandidate>> {
    let store_path = context.store.skill_dir(&discovered.skill_id);
    let store_metadata = match std::fs::symlink_metadata(&store_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("failed to inspect {store_path}")),
    };
    let record = lock.skills.get(&discovered.skill_id);
    let all_locations = || {
        discovered
            .copies
            .iter()
            .flat_map(|copy| copy.locations.iter().cloned())
            .collect::<Vec<_>>()
    };
    let all_canonical_locations = || {
        discovered
            .copies
            .iter()
            .flat_map(|copy| {
                copy.locations
                    .iter()
                    .map(|(_, path)| (path.clone(), copy.canonical.clone()))
            })
            .collect::<BTreeMap<_, _>>()
    };
    let conflict = |reason: String| {
        let first = &discovered.copies[0];
        Some(MigrationCandidate {
            skill_id: discovered.skill_id.clone(),
            skill: first.skill.clone(),
            digest: first.digest.clone(),
            locations: all_locations(),
            canonical_locations: all_canonical_locations(),
            source: owned_migration_source(),
            source_reason: "unresolved conflict".to_string(),
            disposition: MigrationDisposition::Conflict { reason },
        })
    };

    match (record, store_metadata) {
        (Some(record), Some(metadata)) if metadata.is_dir() => {
            let store_digest = digest_skill_tree(&store_path)?;
            let store_canonical = canonical_utf8(&store_path)?;
            let external = discovered
                .copies
                .iter()
                .filter(|copy| copy.canonical != store_canonical)
                .collect::<Vec<_>>();
            if let Some(different) = external
                .iter()
                .find(|copy| copy.digest != store_digest)
            {
                return Ok(conflict(format!(
                    "source {} and managed store {} have different contents; resolve the copies manually before retrying",
                    different.locations[0].1, store_path
                )));
            }
            if external.is_empty() {
                return Ok(None);
            }
            let first = external[0];
            Ok(Some(MigrationCandidate {
                skill_id: discovered.skill_id,
                skill: first.skill.clone(),
                digest: store_digest.clone(),
                locations: external
                    .into_iter()
                    .flat_map(|copy| copy.locations.iter().cloned())
                    .collect(),
                canonical_locations: discovered
                    .copies
                    .iter()
                    .filter(|copy| copy.canonical != store_canonical)
                    .flat_map(|copy| {
                        copy.locations
                            .iter()
                            .map(|(_, path)| (path.clone(), copy.canonical.clone()))
                    })
                    .collect(),
                source: record.source.clone(),
                source_reason: "from existing lockfile".to_string(),
                disposition: MigrationDisposition::Adopt {
                    record: Box::new(record.clone()),
                    store_digest,
                    store_path,
                },
            }))
        }
        (Some(_), Some(_)) => Ok(conflict(format!(
            "lockfile records the skill, but stored path {store_path} is not a directory; restore a valid store directory or remove the stale lock entry"
        ))),
        (Some(_), None) => Ok(conflict(format!(
            "lockfile records the skill, but stored directory {store_path} is missing; restore the store or remove the stale lock entry"
        ))),
        (None, Some(_)) => Ok(conflict(format!(
            "stored path {store_path} exists without a lockfile record; recover or remove the unmanaged store path before retrying"
        ))),
        (None, None) => {
            let first = &discovered.copies[0];
            if let Some(different) = discovered
                .copies
                .iter()
                .find(|copy| copy.digest != first.digest)
            {
                return Ok(conflict(format!(
                    "copies {} and {} have different contents; resolve the copies manually before retrying",
                    first.locations[0].1, different.locations[0].1
                )));
            }
            Ok(Some(MigrationCandidate {
                skill_id: discovered.skill_id,
                skill: first.skill.clone(),
                digest: first.digest.clone(),
                locations: all_locations(),
                canonical_locations: all_canonical_locations(),
                source: owned_migration_source(),
                source_reason: "no known source".to_string(),
                disposition: MigrationDisposition::New,
            }))
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MigrationJournal {
    initializing: bool,
    lock_existed: bool,
    original_lock: LockFile,
    changes: Vec<JournalChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalChange {
    skill_id: SkillId,
    locations: Vec<JournalLocation>,
    store_created: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalLocation {
    source: Utf8PathBuf,
    backup: Utf8PathBuf,
    expected_target: Utf8PathBuf,
    original_was_symlink: bool,
    // Pin the parent, not the skill itself: the skill becomes a symlink during migration.
    resolved_parent: Utf8PathBuf,
}

fn migration_journal_path(context: &ScopeContext) -> Utf8PathBuf {
    context.paths.data_dir.join("migration-journal.json")
}

fn write_migration_journal(context: &ScopeContext, journal: &MigrationJournal) -> Result<()> {
    let path = migration_journal_path(context);
    std::fs::create_dir_all(&context.paths.data_dir)
        .with_context(|| format!("failed to create {}", context.paths.data_dir))?;
    let content = serde_json::to_vec_pretty(journal)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".yasm-migration-journal-")
        .tempfile_in(&context.paths.data_dir)?;
    temporary.write_all(&content)?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path).map_err(|error| error.error)?;
    sync_directory(&context.paths.data_dir)?;
    Ok(())
}

fn validate_migration_candidates(
    context: &ScopeContext,
    candidates: &[MigrationCandidate],
) -> Result<()> {
    if let Some(candidate) = candidates
        .iter()
        .find(|candidate| matches!(candidate.disposition, MigrationDisposition::Conflict { .. }))
    {
        let MigrationDisposition::Conflict { reason } = &candidate.disposition else {
            unreachable!()
        };
        anyhow::bail!(
            "cannot apply migration for `{}`: {reason}. Review the conflict with `--action review`, or select unrelated skills with `--skill <name>`",
            candidate.skill_id
        );
    }

    let lock = LockFile::read(&context.paths.lock_file())?;
    for candidate in candidates {
        let mut seen = BTreeSet::new();
        for (_, location) in &candidate.locations {
            let canonical = canonical_utf8(location)
                .with_context(|| format!("migration source {location} changed after review"))?;
            if candidate.canonical_locations.get(location) != Some(&canonical) {
                anyhow::bail!(
                    "cannot migrate `{}`: source {} resolves to a different location than it did during review; run migration review again",
                    candidate.skill_id,
                    location
                );
            }
            if !seen.insert(canonical) {
                continue;
            }
            let digest = digest_skill_tree(location)
                .with_context(|| format!("failed to recheck migration source {location}"))?;
            if digest != candidate.digest {
                anyhow::bail!(
                    "cannot migrate `{}`: source {} changed after review; run migration review again",
                    candidate.skill_id,
                    location
                );
            }
        }

        let store_path = context.store.skill_dir(&candidate.skill_id);
        match &candidate.disposition {
            MigrationDisposition::New => {
                if lock.skills.contains_key(&candidate.skill_id)
                    || std::fs::symlink_metadata(&store_path).is_ok()
                {
                    anyhow::bail!(
                        "cannot migrate `{}`: managed destination state changed after review; run migration review again",
                        candidate.skill_id
                    );
                }
            }
            MigrationDisposition::Adopt {
                record,
                store_digest,
                store_path: planned_store,
            } => {
                if planned_store != &store_path
                    || lock.skills.get(&candidate.skill_id) != Some(record.as_ref())
                {
                    anyhow::bail!(
                        "cannot adopt `{}`: lockfile state changed after review; run migration review again",
                        candidate.skill_id
                    );
                }
                let metadata = std::fs::symlink_metadata(&store_path).with_context(|| {
                    format!(
                        "cannot adopt `{}`: stored directory is missing",
                        candidate.skill_id
                    )
                })?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    anyhow::bail!(
                        "cannot adopt `{}`: stored path {store_path} is not a directory",
                        candidate.skill_id
                    );
                }
                let current_store_digest = digest_skill_tree(&store_path)?;
                if &current_store_digest != store_digest {
                    anyhow::bail!(
                        "cannot adopt `{}`: stored content changed after review; run migration review again",
                        candidate.skill_id
                    );
                }
                let store_canonical = canonical_utf8(&store_path)?;
                for (_, location) in &candidate.locations {
                    if canonical_utf8(location)? == store_canonical {
                        anyhow::bail!(
                            "cannot adopt `{}` from {location}: source resolves to the managed store itself",
                            candidate.skill_id
                        );
                    }
                }
            }
            MigrationDisposition::Conflict { .. } => unreachable!("conflicts checked above"),
        }
    }
    Ok(())
}

fn apply_migration(
    context: &ScopeContext,
    candidates: Vec<MigrationCandidate>,
    initializing: bool,
) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    validate_migration_candidates(context, &candidates)?;
    let mut lock = LockFile::read(&context.paths.lock_file())?;
    let mut journal = MigrationJournal {
        initializing,
        lock_existed: context.paths.lock_file().exists(),
        original_lock: lock.clone(),
        changes: Vec::new(),
    };

    for candidate in candidates {
        let store_created = matches!(candidate.disposition, MigrationDisposition::New);
        let mut journal_locations = Vec::new();
        let mut seen_locations = BTreeSet::new();
        for (agent_id, location) in &candidate.locations {
            let parent = location.parent().context("skill path has no parent")?;
            let resolved_parent = canonical_utf8(parent)?;
            if !seen_locations.insert(resolved_parent.join(candidate.skill_id.as_str())) {
                continue;
            }
            let backup_root = tempfile::Builder::new()
                .prefix(".yasm-migrate-backup-")
                .tempdir_in(parent)?
                .keep();
            let backup_root = Utf8PathBuf::from_path_buf(backup_root)
                .map_err(|path| anyhow::anyhow!("non-UTF-8 backup path: {}", path.display()))?;
            let agent = context.registry.get(agent_id.as_str())?;
            let original_was_symlink = std::fs::symlink_metadata(location)
                .with_context(|| format!("failed to inspect {location}"))?
                .file_type()
                .is_symlink();
            journal_locations.push(JournalLocation {
                source: location.clone(),
                backup: backup_root.join("skill"),
                expected_target: context
                    .lifecycle()
                    .skill_link_target(agent, &candidate.skill_id)?,
                original_was_symlink,
                resolved_parent,
            });
        }
        journal.changes.push(JournalChange {
            skill_id: candidate.skill_id.clone(),
            locations: journal_locations.clone(),
            store_created,
        });
        write_migration_journal(context, &journal)?;

        if store_created {
            match context
                .store
                .install_skill_dir(&candidate.skill_id, &candidate.skill.directory)
            {
                Ok(_) => {}
                Err(error) => {
                    recover_interrupted_migration(context)?;
                    return Err(anyhow::Error::from(error).context(format!(
                        "failed to stage `{}`; original skills were restored",
                        candidate.skill_id
                    )));
                }
            }
        }
        let result: Result<()> = (|| {
            for location in &journal_locations {
                std::fs::rename(&location.source, &location.backup)
                    .with_context(|| format!("failed to stage {}", location.source))?;
                ensure_skill_symlink(&location.source, &location.expected_target)?;
            }
            let record = match &candidate.disposition {
                MigrationDisposition::New => LockedSkillRecord {
                    name: candidate.skill.name.clone(),
                    source: candidate.source.clone(),
                    resolved: None,
                    skill_path: SkillPath::parse("SKILL.md")?,
                    digest: candidate.digest.clone(),
                    enabled: candidate
                        .locations
                        .iter()
                        .map(|(id, _)| id.clone())
                        .collect(),
                },
                MigrationDisposition::Adopt { record, .. } => {
                    let mut record = record.as_ref().clone();
                    record.enabled.extend(
                        candidate
                            .locations
                            .iter()
                            .map(|(agent_id, _)| agent_id.clone()),
                    );
                    record
                }
                MigrationDisposition::Conflict { .. } => {
                    unreachable!("preflight rejects conflicts")
                }
            };
            lock.skills.insert(candidate.skill_id.clone(), record);
            lock.write(&context.paths.lock_file())?;
            Ok(())
        })();

        if let Err(error) = result {
            recover_interrupted_migration(context)?;
            return Err(error.context(format!(
                "failed to migrate `{}`; original skills were restored",
                candidate.skill_id
            )));
        }
        if store_created {
            println!("migrated {}", candidate.skill_id);
        } else {
            println!("adopted {} using existing store", candidate.skill_id);
        }
    }

    std::fs::remove_file(migration_journal_path(context))
        .context("failed to commit the completed migration")?;
    sync_directory(&context.paths.data_dir)?;
    for change in &journal.changes {
        for location in &change.locations {
            if let Some(root) = location.backup.parent() {
                if let Err(error) = std::fs::remove_dir_all(root) {
                    eprintln!("warning: migrated successfully but could not remove backup {root}: {error}");
                }
            }
        }
    }
    Ok(())
}

fn recover_interrupted_migration(context: &ScopeContext) -> Result<()> {
    let journal_path = migration_journal_path(context);
    if !journal_path.exists() {
        return Ok(());
    }
    let journal: MigrationJournal = serde_json::from_slice(
        &std::fs::read(&journal_path)
            .with_context(|| format!("failed to read recovery journal {journal_path}"))?,
    )
    .with_context(|| format!("failed to parse recovery journal {journal_path}"))?;
    validate_migration_journal(context, &journal)?;

    for change in journal.changes.iter().rev() {
        for location in change.locations.iter().rev() {
            let backup_exists = match std::fs::symlink_metadata(&location.backup) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            if backup_exists {
                match std::fs::symlink_metadata(&location.source) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let target = std::fs::read_link(&location.source)?;
                        if target != location.expected_target.as_std_path() {
                            anyhow::bail!(
                                "cannot recover {}: link target changed to {} (backup preserved at {})",
                                location.source,
                                target.display(),
                                location.backup
                            );
                        }
                        std::fs::remove_file(&location.source)?;
                    }
                    Ok(_) => anyhow::bail!(
                        "cannot recover {}: path was recreated (backup preserved at {})",
                        location.source,
                        location.backup
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                std::fs::rename(&location.backup, &location.source).with_context(|| {
                    format!(
                        "failed to restore {} from {}",
                        location.source, location.backup
                    )
                })?;
            }
            if let Some(root) = location.backup.parent() {
                let _ = std::fs::remove_dir(root);
            }
        }
        if change.store_created {
            context.store.remove_skill(&change.skill_id)?;
        }
    }
    if journal.lock_existed {
        journal.original_lock.write(&context.paths.lock_file())?;
    } else if context.paths.lock_file().exists() {
        std::fs::remove_file(context.paths.lock_file())?;
    }
    std::fs::remove_file(&journal_path)?;
    if journal.initializing {
        remove_empty_initialized_store(context)?;
    }
    eprintln!("recovered an interrupted migration; original skills were restored");
    Ok(())
}

fn remove_empty_initialized_store(context: &ScopeContext) -> Result<()> {
    let skills_dir = context.paths.skills_dir();
    let skills_empty = match std::fs::read_dir(&skills_dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    if !skills_empty {
        return Ok(());
    }
    let has_unrelated_entries = std::fs::read_dir(&context.paths.data_dir)?
        .filter_map(std::result::Result::ok)
        .any(|entry| !matches!(entry.file_name().to_str(), Some("skills" | "yasm.lock")));
    if has_unrelated_entries {
        return Ok(());
    }

    if context.paths.lock_file().exists() {
        std::fs::remove_file(context.paths.lock_file())?;
    }
    if skills_dir.exists() {
        std::fs::remove_dir(&skills_dir)?;
    }
    match std::fs::remove_dir(&context.paths.data_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn validate_migration_journal(context: &ScopeContext, journal: &MigrationJournal) -> Result<()> {
    if journal.initializing && !context.is_project() {
        anyhow::bail!("invalid migration journal: global migration cannot initialize a project");
    }
    if journal.initializing && !journal.original_lock.skills.is_empty() {
        anyhow::bail!("invalid migration journal: initialized project lock was not empty");
    }
    let root = context.migration_root()?;
    let mut seen = BTreeSet::new();
    for change in &journal.changes {
        if !seen.insert(change.skill_id.clone()) {
            anyhow::bail!(
                "invalid migration journal: duplicate skill `{}`",
                change.skill_id
            );
        }
        let existed_before = journal.original_lock.skills.contains_key(&change.skill_id);
        if change.store_created == existed_before {
            anyhow::bail!(
                "invalid migration journal: store ownership for `{}` does not match the original lockfile",
                change.skill_id
            );
        }
        for location in &change.locations {
            let agent = context
                .registry
                .all()
                .iter()
                .find(|agent| agent.skill_link(&change.skill_id) == location.source)
                .with_context(|| {
                    format!(
                        "invalid migration journal: source {} is not a registered agent path",
                        location.source
                    )
                })?;
            let actual_parent = canonical_utf8(&agent.skill_dir)?;
            if actual_parent != location.resolved_parent {
                anyhow::bail!(
                    "cannot recover: agent directory {} changed target from {} to {}; backups preserved",
                    agent.skill_dir,
                    location.resolved_parent,
                    actual_parent
                );
            }
            if context.is_project()
                && agent_directory_has_symlink_component(&root, &agent.skill_dir)?
            {
                anyhow::bail!(
                    "cannot recover through symlinked agent directory {}",
                    agent.skill_dir
                );
            }
            let backup_root = location
                .backup
                .parent()
                .context("invalid migration journal: backup has no parent")?;
            if location.backup.file_name() != Some("skill")
                || backup_root.parent() != Some(agent.skill_dir.as_path())
                || !backup_root
                    .file_name()
                    .is_some_and(|name| name.starts_with(".yasm-migrate-backup-"))
            {
                anyhow::bail!(
                    "invalid migration journal: backup {} is outside the registered agent directory",
                    location.backup
                );
            }
            if let Ok(metadata) = std::fs::symlink_metadata(backup_root) {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    anyhow::bail!(
                        "invalid migration journal: backup root {backup_root} is not a directory"
                    );
                }
            }
            match std::fs::symlink_metadata(&location.backup) {
                Ok(metadata)
                    if location.original_was_symlink && metadata.file_type().is_symlink() => {}
                Ok(metadata)
                    if !location.original_was_symlink
                        && metadata.is_dir()
                        && !metadata.file_type().is_symlink() => {}
                Ok(_) => anyhow::bail!(
                    "invalid migration journal: backup {} has an unexpected file type",
                    location.backup
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::symlink_metadata(&location.source) {
                        Ok(metadata)
                            if location.original_was_symlink
                                && metadata.file_type().is_symlink() => {}
                        Ok(metadata)
                            if !location.original_was_symlink
                                && metadata.is_dir()
                                && !metadata.file_type().is_symlink() => {}
                        _ => anyhow::bail!(
                            "invalid migration journal: {} has neither its original path type nor a backup",
                            location.source
                        ),
                    }
                }
                Err(error) => return Err(error.into()),
            }
            let expected = context
                .lifecycle()
                .skill_link_target(agent, &change.skill_id)?;
            if location.expected_target != expected {
                anyhow::bail!(
                    "invalid migration journal: unexpected link target for {}",
                    location.source
                );
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Utf8Path) -> Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Utf8Path) -> Result<()> {
    Ok(())
}

fn print_project_commit_hint(context: &ScopeContext) {
    if let ResolvedScope::Project { root } = &context.scope {
        if !is_git_work_tree(root) {
            return;
        }
        println!("Commit .yasm/ and the agent skill links to version control.");
    }
}

fn add(
    context: &ScopeContext,
    source_input: SourceInput,
    skill_selection: Option<&str>,
    agent_filter: &[LinkTarget],
    no_enable: bool,
    requested_action: Option<ChangeAction>,
    replace: bool,
) -> Result<()> {
    let mut source = source_input.into_spec()?;
    let bundled = source.kind == SourceKind::Bundled;
    if bundled && skill_selection.is_some() {
        anyhow::bail!(
            "the `self` bundle is installed as a unit; remove `--skill` and run `yasm add self`"
        );
    }
    let mut lock = LockFile::read(&context.paths.lock_file())?;
    let progress = progress::Progress::start("Fetching source ...", false);
    let fetched = fetch_source_cached(&source, &context.paths.cache_dir.join("sources"))?;
    progress.set_message("Discovering skills ...");
    let discovery = discover_skills_with_diagnostics(&fetched.root)?;
    warn_skipped_skills(&progress, &discovery.skipped);
    progress.finish_and_clear();
    let mut discovered = discovery.skills;
    if discovered.is_empty() {
        if let Some(subpath) = &source.subpath {
            anyhow::bail!(
                "no valid skills found in Git directory `{}`",
                subpath.as_str()
            );
        }
    }
    // Receipts use repository-relative paths, regardless of the discovery filter.
    for skill in &mut discovered {
        skill.skill_path = add_plan::repository_skill_path(&source, &skill.skill_path)?;
    }
    source.subpath = None;
    let bundle = bundled
        .then(|| SelfBundle::from_skills(&discovered))
        .transpose()?;
    let plan = add_plan::plan_skills(&discovered, &source, fetched.resolved.as_ref(), &lock)?;
    let selected = if bundled {
        plan.iter().collect::<Vec<_>>()
    } else {
        select_add_skills(&plan, skill_selection)?
    };
    let skills = selected
        .iter()
        .map(|planned| planned.skill)
        .collect::<Vec<_>>();
    if skills.is_empty() {
        return Ok(());
    }
    validate_reserved_skill_ids(&skills, bundled)?;
    let mut selected_ids = BTreeMap::new();
    for planned in &selected {
        if let Some(previous) =
            selected_ids.insert(&planned.skill_id, planned.skill.skill_path.as_str())
        {
            anyhow::bail!("selected skills share installation ID `{}`; conflicting paths: {previous}, {}; choose one with --skill <name-or-path>",
                planned.skill_id, planned.skill.skill_path.as_str());
        }
    }
    if requested_action != Some(ChangeAction::Skip) {
        for planned in &selected {
            if planned.state == add_plan::AddSkillState::Conflict && !replace && !bundled {
                let existing = lock
                    .skills
                    .get(&planned.skill_id)
                    .context("planned source conflict has no installation receipt")?;
                anyhow::bail!(
                    "skill `{}` is already acquired from {} at `{}`; pass --replace to replace its upstream source",
                    planned.skill_id, display_source_root(existing), existing.skill_path.as_str()
                );
            }
        }
    }
    let agents = select_add_agents(&context.registry, agent_filter, no_enable)?;

    if bundled {
        for skill in &skills {
            let skill_id = sanitize_skill_id(&skill.name)?;
            if let Some(existing) = lock.skills.get(&skill_id) {
                if existing.source.kind != SourceKind::Bundled
                    || existing.source.path != SELF_BUNDLE_ID
                {
                    anyhow::bail!(
                        "cannot install bundled skill `{skill_id}` because that ID is already acquired from {}; remove it first",
                        display_source_root(existing)
                    );
                }
            } else if context.store.skill_dir(&skill_id).exists() {
                anyhow::bail!(
                    "cannot install bundled skill `{skill_id}` because the store path already exists without a lock record"
                );
            }
        }
    }

    if requested_action == Some(ChangeAction::Skip) {
        for skill in skills {
            println!("skipped {}", sanitize_skill_id(&skill.name)?);
        }
        return Ok(());
    }

    let mut managed_candidates = Vec::new();
    let mut install_candidates = Vec::new();
    for planned in selected {
        let skill = planned.skill;
        let skill_id = planned.skill_id.clone();
        let existing_record = lock.skills.get(&skill_id).cloned();
        let mut candidate_agents = agents.clone();
        if let Some(record) = &existing_record {
            for id in &record.enabled {
                if !candidate_agents.iter().any(|agent| &agent.id == id) {
                    candidate_agents.push(context.registry.get(id.as_str())?);
                }
            }
        }
        // Retained links must be checked before either files or receipts change.
        let replacements = add_replacement_targets(context, &candidate_agents, &skill_id, replace)?;
        if replacements.is_empty() {
            if let Some(existing_record) = existing_record {
                managed_candidates.push(AddManagedCandidate {
                    skill_id,
                    skill,
                    existing_record,
                    same_upstream: planned.state == add_plan::AddSkillState::Installed,
                    agents: candidate_agents,
                });
                continue;
            }
        }
        install_candidates.push(AddInstallCandidate {
            skill_id,
            skill,
            replacements,
            agents: candidate_agents,
        });
    }

    let mut unchanged_updates = Vec::new();
    let mut prepared_managed = Vec::new();
    for managed in managed_candidates {
        let installed_path = context.store.skill_dir(&managed.skill_id);
        let mut diff = skill_source_directory_diff(
            &managed.skill_id,
            &installed_path,
            &managed.skill.directory,
            source.kind.is_git(),
        )?;
        let mut locked_agents = managed.existing_record.enabled.clone();
        locked_agents.extend(agent_ids(&managed.agents));

        let same_upstream = managed.same_upstream;
        if !same_upstream {
            diff.changed = true;
            let source_change = format!(
                "upstream source: {} @{} [{}] -> {} @{} [{}]\n",
                display_source_root(&managed.existing_record),
                managed
                    .existing_record
                    .source
                    .r#ref
                    .as_ref()
                    .map_or("default", GitRef::as_str),
                source_skill_path(&managed.existing_record),
                display_user_path(&source.path),
                source.r#ref.as_ref().map_or("default", GitRef::as_str),
                managed.skill.skill_path.as_str()
            );
            diff.output.insert_str(0, &source_change);
            if let Some(output) = &mut diff.terminal_output {
                output.insert_str(0, &source_change);
            }
        }
        let owned_agents = managed
            .existing_record
            .enabled
            .iter()
            .map(|id| context.registry.get(id.as_str()))
            .collect::<yasm_core::Result<Vec<_>>>()?;
        let links_healthy = owned_agents.iter().all(|agent| {
            let Ok(expected) = context
                .lifecycle()
                .skill_link_target(agent, &managed.skill_id)
            else {
                return false;
            };
            std::fs::read_link(agent.skill_link(&managed.skill_id))
                .is_ok_and(|target| target == expected.as_std_path())
        });
        if !diff.changed
            && locked_agents == managed.existing_record.enabled
            && same_upstream
            && links_healthy
        {
            if let Some(record) = lock.skills.get_mut(&managed.skill_id) {
                record.source = source.clone();
                record.skill_path = managed.skill.skill_path.clone();
                record.resolved = fetched.resolved.clone();
                record.digest = digest_skill_tree(&installed_path)?;
            }
            lock.write(&context.paths.lock_file())?;
            unchanged_updates.push(managed.skill_id);
            continue;
        }
        prepared_managed.push(PreparedManagedAddCandidate { managed, diff });
    }
    print_unchanged_add_updates(&unchanged_updates);

    let bundled_retirements = if bundled && lock.bundles.contains_key(SELF_BUNDLE_ID) {
        let current = discovered
            .iter()
            .map(|skill| sanitize_skill_id(&skill.name))
            .collect::<yasm_core::Result<BTreeSet<_>>>()?;
        acquired_self_member_ids(&lock)
            .difference(&current)
            .cloned()
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    let effective_action = if bundled
        && (!prepared_managed.is_empty()
            || !install_candidates.is_empty()
            || !bundled_retirements.is_empty())
    {
        let action = match requested_action {
            Some(action) => action,
            None => select_change_action(
                "self bundle action",
                "How do you want to install or update the `self` bundle?",
                "Review changes",
                "Apply without review",
                "pass --action apply to install without prompting, or --action skip to skip",
            )?,
        };
        if action == ChangeAction::Review {
            for candidate in &prepared_managed {
                if candidate.diff.changed {
                    print_skill_diff(&candidate.managed.skill_id, &candidate.diff, false);
                }
            }
            for candidate in &install_candidates {
                let diff = add_candidate_diff(
                    &candidate.skill_id,
                    candidate.skill,
                    &candidate.replacements,
                    source.kind.is_git(),
                )?;
                print_add_diff(&candidate.skill_id, &diff);
            }
            for skill_id in &bundled_retirements {
                let modified = lock
                    .skills
                    .get(skill_id)
                    .and_then(|record| {
                        digest_skill_tree(&context.store.skill_dir(skill_id))
                            .ok()
                            .map(|digest| digest != record.digest)
                    })
                    .unwrap_or(false);
                let suffix = if modified { " (locally modified)" } else { "" };
                println!("bundle member to retire: {skill_id}{suffix}");
            }
            if interactive::ask_confirm(
                "self bundle confirmation",
                "Apply all changes for the `self` bundle?",
                false,
                "pass --action apply to install without prompting, or --action skip to skip",
            )? {
                Some(ChangeAction::Apply)
            } else {
                Some(ChangeAction::Skip)
            }
        } else {
            Some(action)
        }
    } else {
        requested_action
    };

    if effective_action == Some(ChangeAction::Skip) {
        for candidate in &prepared_managed {
            println!("skipped {}", candidate.managed.skill_id);
        }
        for candidate in &install_candidates {
            println!("skipped {}", candidate.skill_id);
        }
        return Ok(());
    }

    let selected_managed =
        select_add_managed_update_candidates(prepared_managed, effective_action)?;
    let mut changed_updates = Vec::new();
    for prepared in selected_managed {
        let managed = prepared.managed;
        if !prepared.diff.changed {
            context
                .lifecycle()
                .enable(&mut lock, &managed.skill_id, &managed.agents, false)?;
            if let Some(record) = lock.skills.get_mut(&managed.skill_id) {
                record.source = source.clone();
                record.skill_path = managed.skill.skill_path.clone();
                record.resolved = fetched.resolved.clone();
                record.digest = digest_skill_tree(&context.store.skill_dir(&managed.skill_id))?;
            }
            lock.write(&context.paths.lock_file())?;
            println!("updated {}", managed.skill_id);
            continue;
        }

        changed_updates.push(PreparedAddUpdate {
            candidate: UpdateCandidate {
                skill_id: managed.skill_id.clone(),
                record: LockedSkillRecord {
                    name: managed.skill.name.clone(),
                    source: source.clone(),
                    resolved: fetched.resolved.clone(),
                    skill_path: managed.skill.skill_path.clone(),
                    digest: managed.existing_record.digest.clone(),
                    enabled: managed.existing_record.enabled,
                },
                fetched: fetched.clone(),
                selected_skill: (*managed.skill).clone(),
                diff: prepared.diff,
            },
            agents: managed.agents,
        });
    }

    if !changed_updates.is_empty() {
        let action = match effective_action {
            Some(action) => action,
            None => select_update_action()?,
        };
        for prepared in changed_updates {
            if apply_update_action(&prepared.candidate, action, false)? {
                let skill_id = prepared.candidate.skill_id.clone();
                apply_update_candidate(
                    context,
                    &mut lock,
                    prepared.candidate,
                    &prepared.agents,
                    false,
                )?;
                println!("updated {}", skill_id);
            } else {
                println!("skipped {}", prepared.candidate.skill_id);
            }
        }
    }

    if !install_candidates.is_empty() {
        let action = match effective_action {
            Some(action) => action,
            None => select_add_action(&install_candidates)?,
        };
        for candidate in install_candidates {
            if action == ChangeAction::Skip {
                println!("skipped {}", candidate.skill_id);
                continue;
            }
            let apply = match action {
                ChangeAction::Review => {
                    let diff = add_candidate_diff(
                        &candidate.skill_id,
                        candidate.skill,
                        &candidate.replacements,
                        source.kind.is_git(),
                    )?;
                    print_add_diff(&candidate.skill_id, &diff);
                    interactive::ask_confirm(
                        "add confirmation",
                        add_confirmation_prompt(
                            &candidate.skill_id,
                            !candidate.replacements.is_empty(),
                        )
                        .as_str(),
                        false,
                        "pass --action apply to install without prompting, or --action skip to skip",
                    )?
                }
                ChangeAction::Apply => true,
                ChangeAction::Skip => unreachable!("skip handled before add conflict checks"),
            };
            if !apply {
                println!("skipped {}", candidate.skill_id);
                continue;
            }

            apply_install_candidate(context, &mut lock, &source, &fetched, candidate)?;
        }
    }

    if let Some(bundle) = &bundle {
        let installed = acquired_self_member_ids(&lock);
        let complete = bundle.members().is_subset(&installed);
        if complete && lock.bundles.contains_key(SELF_BUNDLE_ID) {
            reconcile_self_bundle(context, &mut lock, bundle, &source, effective_action, false)?;
        }
        refresh_self_bundle_receipt(context, &mut lock, bundle, &agents)?;
    }

    Ok(())
}

fn validate_reserved_skill_ids(discovered: &[&DiscoveredSkill], bundled: bool) -> Result<()> {
    if bundled {
        return Ok(());
    }
    if discovered
        .iter()
        .any(|skill| sanitize_skill_id(&skill.name).is_ok_and(|id| id.as_str() == SELF_BUNDLE_ID))
    {
        anyhow::bail!(
            "skill ID `self` is reserved for Yasm bundled skills; rename the skill before acquiring it"
        );
    }
    Ok(())
}

fn refresh_self_bundle_receipt(
    context: &ScopeContext,
    lock: &mut LockFile,
    bundle: &SelfBundle<'_>,
    agents: &[&Agent],
) -> Result<()> {
    let members = bundle.members();
    let installed = acquired_self_member_ids(lock);
    if installed.is_empty() {
        return Ok(());
    }

    if !bundle.matches_installed_contents(context, &installed)? {
        return Ok(());
    }

    let previous = lock.bundles.get(SELF_BUNDLE_ID).cloned();
    let mut enabled = previous
        .as_ref()
        .map(|record| record.enabled.clone())
        .unwrap_or_default();
    enabled.extend(agent_ids(agents));
    let mut excluded = previous
        .as_ref()
        .map(|record| record.excluded.clone())
        .unwrap_or_default();
    // Explicitly adding the running bundle reinstalls its current members, but
    // must not forget tombstones for members only a different executable knows.
    for skill_id in &installed {
        excluded.remove(skill_id);
    }
    excluded.extend(members.difference(&installed).cloned());
    bundle.write_receipt(context, lock, previous.as_ref(), excluded, enabled)
}

fn apply_install_candidate(
    context: &ScopeContext,
    lock: &mut LockFile,
    source: &SourceSpec,
    fetched: &FetchedSource,
    candidate: AddInstallCandidate<'_>,
) -> Result<()> {
    let agents = &candidate.agents;
    let skill_id = candidate.skill_id;
    let install_progress = progress::Progress::start(format!("Acquiring {skill_id} ..."), false);
    let lifecycle = context.lifecycle();
    lifecycle.acquire(
        lock,
        &skill_id,
        candidate.skill.name.clone(),
        source.clone(),
        fetched.resolved.clone(),
        candidate.skill.skill_path.clone(),
        &candidate.skill.directory,
    )?;
    if agents.is_empty() {
        install_progress.finish_and_clear();
        println!("acquired {skill_id}");
    } else {
        let replace = !candidate.replacements.is_empty();
        lifecycle.enable(lock, &skill_id, agents, replace)?;
        install_progress.finish_and_clear();
        println!("installed {skill_id} for {}", display_agent_ids(agents));
    }
    Ok(())
}

fn list(
    contexts: &[ScopeContext],
    agent: Option<LinkTarget>,
    enabled_only: bool,
    json: bool,
) -> Result<()> {
    let mut has_acquired_skills = false;
    let mut visible = Vec::new();
    for context in contexts {
        let mut lock = LockFile::read(&context.paths.lock_file())?;
        let mut plugins = plugins_for_list(context)?;
        has_acquired_skills |= !lock.skills.is_empty() || !plugins.is_empty();
        let selected_agent = agent
            .map(|id| context.registry.get(id.as_str()))
            .transpose()?;
        lock.skills.retain(|_, record| {
            if enabled_only && record.enabled.is_empty() {
                return false;
            }
            selected_agent.is_none_or(|agent| record.enabled.contains(&agent.id))
        });
        if let Some(agent) = agent {
            let target = match agent.as_str() {
                "universal" => "codex",
                target => target,
            };
            plugins.retain_mut(|plugin| {
                if !plugin.enabled.iter().any(|enabled| enabled == target) {
                    return false;
                }
                plugin.enabled.retain(|enabled| enabled == target);
                plugin.skills.retain_mut(|skill| {
                    skill.enabled.retain(|enabled| enabled == target);
                    !skill.enabled.is_empty()
                });
                plugin.mcp_servers.retain_mut(|server| {
                    server.enabled.retain(|enabled| enabled == target);
                    !server.enabled.is_empty()
                });
                true
            });
        } else if enabled_only {
            plugins.retain(|plugin| !plugin.enabled.is_empty());
        }
        visible.push((context, lock, plugins));
    }

    if json {
        let project = visible
            .iter()
            .find(|(context, _, _)| context.is_project())
            .map(|(context, lock, plugins)| list_scope_json(context, lock, plugins))
            .transpose()?;
        let global = visible
            .iter()
            .find(|(context, _, _)| context.is_global())
            .map(|(context, lock, plugins)| list_scope_json(context, lock, plugins))
            .transpose()?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "project": project,
                "global": global,
            }))?
        );
        return Ok(());
    }

    if !has_acquired_skills {
        println!("no skills acquired");
        return Ok(());
    }

    if visible
        .iter()
        .all(|(_, lock, plugins)| lock.skills.is_empty() && plugins.is_empty())
    {
        if let Some(agent) = agent {
            println!("no skills enabled for {}", agent.as_str());
        } else {
            println!("no skills enabled");
        }
        return Ok(());
    }

    let mut printed = false;
    for (context, lock, plugins) in &visible {
        if !lock.skills.is_empty() {
            if printed {
                println!();
            }
            println!("{} ({})\n", context.list_title(), lock.skills.len());
            print_skill_table(context, &lock.skills.iter().collect::<Vec<_>>());
            printed = true;
        }
        if !plugins.is_empty() {
            if printed {
                println!();
            }
            let title = match &context.scope {
                ResolvedScope::Project { .. } => "Project Plugins",
                ResolvedScope::Global => "Global Plugins",
            };
            println!("{title} ({})\n", plugins.len());
            print_plugin_table(plugins);
            printed = true;
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct ListedPluginComponent {
    name: String,
    enabled: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ListedPlugin {
    id: String,
    description: Option<String>,
    enabled: Vec<String>,
    skill_count: usize,
    mcp_server_count: usize,
    skills: Vec<ListedPluginComponent>,
    mcp_servers: Vec<ListedPluginComponent>,
}

fn plugins_for_list(context: &ScopeContext) -> Result<Vec<ListedPlugin>> {
    #[cfg(feature = "marketplace")]
    {
        let global = context.is_global();
        Ok(yasm_marketplace::installed_plugin_summaries(global)?
            .into_iter()
            .map(|plugin| ListedPlugin {
                id: plugin.id,
                description: plugin.description,
                enabled: plugin.enabled,
                skill_count: plugin.skill_count,
                mcp_server_count: plugin.mcp_server_count,
                skills: plugin
                    .skills
                    .into_iter()
                    .map(|skill| ListedPluginComponent {
                        name: skill.name,
                        enabled: skill.enabled,
                    })
                    .collect(),
                mcp_servers: plugin
                    .mcp_servers
                    .into_iter()
                    .map(|server| ListedPluginComponent {
                        name: server.name,
                        enabled: server.enabled,
                    })
                    .collect(),
            })
            .collect())
    }
    #[cfg(not(feature = "marketplace"))]
    {
        let _ = context;
        Ok(Vec::new())
    }
}

fn list_scope_json(
    context: &ScopeContext,
    lock: &LockFile,
    plugins: &[ListedPlugin],
) -> Result<serde_json::Value> {
    #[cfg(feature = "marketplace")]
    {
        let mut value = lock_with_metadata_json(context, lock)?;
        value
            .as_object_mut()
            .context("list JSON scope must be an object")?
            .insert("plugins".to_string(), serde_json::to_value(plugins)?);
        Ok(value)
    }
    #[cfg(not(feature = "marketplace"))]
    {
        let _ = plugins;
        lock_with_metadata_json(context, lock)
    }
}

fn print_plugin_table(plugins: &[ListedPlugin]) {
    const SUMMARY_WIDTH: usize = 64;

    fn concise_summary(plugin: &ListedPlugin) -> String {
        let description = plugin
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map(|description| description.split_whitespace().collect::<Vec<_>>().join(" "));
        let summary = description.unwrap_or_else(|| {
            let mut components = plugin
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .chain(plugin.mcp_servers.iter().map(|server| server.name.as_str()));
            let shown = components
                .by_ref()
                .take(3)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            let remaining = components.count();
            if shown.is_empty() {
                "no portable skills or MCP servers".to_string()
            } else if remaining == 0 {
                shown.join(", ")
            } else {
                format!("{} +{remaining} more", shown.join(", "))
            }
        });
        if summary.chars().count() <= SUMMARY_WIDTH {
            summary
        } else {
            format!(
                "{}…",
                summary.chars().take(SUMMARY_WIDTH - 1).collect::<String>()
            )
        }
    }

    fn selection_summary(
        components: &[ListedPluginComponent],
        targets: &[String],
        total: usize,
    ) -> String {
        if targets.is_empty() {
            return format!("0/{total}");
        }
        let counts = targets
            .iter()
            .map(|target| {
                components
                    .iter()
                    .filter(|component| component.enabled.contains(target))
                    .count()
            })
            .collect::<Vec<_>>();
        let minimum = counts.iter().min().copied().unwrap_or(0);
        let maximum = counts.iter().max().copied().unwrap_or(0);
        if minimum == maximum {
            format!("{minimum}/{total}")
        } else {
            format!("mixed ({minimum}–{maximum}/{total})")
        }
    }

    #[derive(Tabled)]
    struct PluginRow<'a> {
        #[tabled(rename = "Plugin")]
        plugin: &'a str,
        #[tabled(rename = "Enabled")]
        enabled: String,
        #[tabled(rename = "Skills")]
        skills: String,
        #[tabled(rename = "MCP")]
        mcp: String,
        #[tabled(rename = "Summary")]
        summary: String,
    }

    let mut table = Table::new(plugins.iter().map(|plugin| PluginRow {
        plugin: &plugin.id,
        enabled: if plugin.enabled.is_empty() {
            "—".to_string()
        } else {
            plugin.enabled.join(", ")
        },
        skills: selection_summary(&plugin.skills, &plugin.enabled, plugin.skill_count),
        mcp: selection_summary(
            &plugin.mcp_servers,
            &plugin.enabled,
            plugin.mcp_server_count,
        ),
        summary: concise_summary(plugin),
    }));
    table.with(
        Style::modern()
            .remove_horizontal()
            .horizontals([(1, HorizontalLine::inherit(Style::modern()))]),
    );
    println!("{table}");
}

fn print_skill_table(context: &ScopeContext, skills: &[(&SkillId, &LockedSkillRecord)]) {
    #[derive(Tabled)]
    struct SourcedSkillRow {
        #[tabled(rename = "Skill")]
        skill: String,
        #[tabled(rename = "Source")]
        source: String,
        #[tabled(rename = "Enabled")]
        enabled: String,
        #[tabled(rename = "Manual only")]
        manual_only: String,
    }

    #[derive(Tabled)]
    struct OwnedSkillRow {
        #[tabled(rename = "Skill")]
        skill: String,
        #[tabled(rename = "Enabled")]
        enabled: String,
        #[tabled(rename = "Manual only")]
        manual_only: String,
    }

    let skills = skills
        .iter()
        .map(|(skill_id, record)| (*skill_id, *record, installed_metadata(context, skill_id)))
        .collect::<Vec<_>>();
    let show_source = skills
        .iter()
        .any(|(_, record, _)| record.source.kind != SourceKind::Owned);
    let mut table = if show_source {
        Table::new(
            skills
                .iter()
                .map(|(skill_id, record, metadata)| SourcedSkillRow {
                    skill: format!(
                        "{:<16}",
                        display_installed_skill_name(skill_id, record, metadata)
                    ),
                    source: display_source_root(record),
                    enabled: display_enabled_agents(record),
                    manual_only: display_manual_only(metadata),
                }),
        )
    } else {
        Table::new(
            skills
                .iter()
                .map(|(skill_id, record, metadata)| OwnedSkillRow {
                    skill: format!(
                        "{:<16}",
                        display_installed_skill_name(skill_id, record, metadata)
                    ),
                    enabled: display_enabled_agents(record),
                    manual_only: display_manual_only(metadata),
                }),
        )
    };
    table.with(
        Style::modern()
            .remove_horizontal()
            .horizontals([(1, HorizontalLine::inherit(Style::modern()))]),
    );
    println!("{table}");
}

fn display_enabled_agents(record: &LockedSkillRecord) -> String {
    if record.enabled.is_empty() {
        "—".to_string()
    } else {
        record
            .enabled
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn display_source_root(record: &LockedSkillRecord) -> String {
    let path = match record.source.kind {
        SourceKind::Bundled => "Yasm bundled skills".to_string(),
        SourceKind::Github | SourceKind::Git => record
            .source
            .path
            .strip_suffix(".git")
            .unwrap_or(&record.source.path)
            .to_string(),
        SourceKind::Local => record.source.path.clone(),
        SourceKind::Owned => "—".to_string(),
    };
    display_user_path(&path)
}

fn source_skill_path(record: &LockedSkillRecord) -> Utf8PathBuf {
    let mut path = Utf8PathBuf::new();
    if let Some(subpath) = &record.source.subpath {
        path.push(subpath.as_str());
    }
    path.push(record.skill_path.as_str());
    path
}

fn info(context: &ScopeContext, skill: &str) -> Result<()> {
    let lock = LockFile::read(&context.paths.lock_file())?;
    if skill == SELF_BUNDLE_ID {
        return info_self_bundle(context, &lock);
    }
    let skill_id = selected_skill_ids(context, &lock, &[skill.to_string()], "info")?
        .into_iter()
        .next()
        .expect("info always selects one requested skill");
    let record = lock
        .skills
        .get(&skill_id)
        .expect("selected skill must exist in the lockfile");
    let metadata = installed_metadata(context, &skill_id);

    println!("Name: {}", metadata.name.as_ref().unwrap_or(&record.name));
    println!("ID: {skill_id}");
    println!(
        "Description: {}",
        metadata.description.as_deref().unwrap_or("—")
    );
    println!(
        "Scope: {}",
        match &context.scope {
            ResolvedScope::Global => "global",
            ResolvedScope::Project { .. } => "project",
        }
    );
    println!("Enabled: {}", display_enabled_agents(record));
    for harness in Harness::ALL {
        println!(
            "{} invocation: {}",
            harness.display_name(),
            metadata.invocation.status(*harness).display_name()
        );
    }
    println!(
        "Installed: {}",
        display_installed_skill_path(context, &skill_id)
    );

    match record.source.kind {
        SourceKind::Bundled => {
            println!("Source type: bundled");
            println!("Bundle: Yasm bundled skills");
            if let Some(bundle) = lock.bundles.get(SELF_BUNDLE_ID) {
                println!("Bundled release: {}", bundle.release);
            }
        }
        SourceKind::Github | SourceKind::Git => {
            println!("Source type: Git");
            println!("Repository: {}", display_source_root(record));
            println!("Skill path: {}", source_skill_path(record));
            if let Some(git_ref) = record.source.r#ref.as_ref().or_else(|| {
                record
                    .resolved
                    .as_ref()
                    .and_then(|resolved| resolved.r#ref.as_ref())
            }) {
                println!("Ref: {}", git_ref.as_str());
            }
            if let Some(resolved) = &record.resolved {
                println!("Resolved commit: {}", resolved.commit);
            }
        }
        SourceKind::Local => {
            println!("Source type: local");
            println!("Source root: {}", display_source_root(record));
            println!(
                "Skill file: {}",
                display_user_path(
                    Utf8Path::new(&record.source.path)
                        .join(record.skill_path.as_str())
                        .as_str()
                )
            );
        }
        SourceKind::Owned => println!("Ownership: locally owned"),
    }
    Ok(())
}

fn info_self_bundle(context: &ScopeContext, lock: &LockFile) -> Result<()> {
    let Some(bundle) = lock.bundles.get(SELF_BUNDLE_ID) else {
        if context.is_project() {
            let global = LockFile::read(&YasmPaths::discover()?.lock_file())?;
            if global.bundles.contains_key(SELF_BUNDLE_ID) {
                anyhow::bail!(
                    "the `self` bundle is not installed in this project, but exists globally.\nRun `yasm info self --global` to inspect it globally."
                );
            }
        }
        anyhow::bail!(
            "the `self` bundle is not acquired; run `yasm add self{} --action apply`",
            if context.is_global() { " --global" } else { "" }
        );
    };
    println!("Name: Yasm bundled skills");
    println!("ID: {SELF_BUNDLE_ID}");
    println!(
        "Scope: {}",
        match &context.scope {
            ResolvedScope::Global => "global",
            ResolvedScope::Project { .. } => "project",
        }
    );
    println!("Installed release: {}", bundle.release);
    println!("Running release: {}", env!("CARGO_PKG_VERSION"));
    println!(
        "Bundle content: {}",
        if bundle.digest == self_bundle_digest() {
            "current"
        } else {
            "differs from the running executable; run `yasm update self --action apply`"
        }
    );
    println!(
        "Default enabled: {}",
        if bundle.enabled.is_empty() {
            "—".to_string()
        } else {
            bundle
                .enabled
                .iter()
                .map(AgentId::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!("Members:");
    for member in &bundle.members {
        let state = if bundle.excluded.contains(member) {
            "removed"
        } else if lock.skills.contains_key(member) {
            "acquired"
        } else {
            "missing"
        };
        println!("  {member}: {state}");
    }
    Ok(())
}

fn display_installed_skill_path(context: &ScopeContext, skill_id: &SkillId) -> String {
    let path = context.store.skill_dir(skill_id).join("SKILL.md");
    if let ResolvedScope::Project { root } = &context.scope {
        if let Ok(relative) = path.strip_prefix(root) {
            return relative.to_string();
        }
    }
    display_user_path(path.as_str())
}

fn display_skill_name(skill_id: &SkillId, record: &LockedSkillRecord) -> String {
    if skill_id.as_str() == record.name.as_str() {
        record.name.to_string()
    } else {
        format!("{} (id: {})", record.name, skill_id)
    }
}

fn display_installed_skill_name(
    skill_id: &SkillId,
    record: &LockedSkillRecord,
    metadata: &SkillMetadata,
) -> String {
    let name = metadata.name.as_ref().unwrap_or(&record.name);
    if skill_id.as_str() == name.as_str() {
        name.to_string()
    } else {
        format!("{name} (id: {skill_id})")
    }
}

fn display_manual_only(metadata: &SkillMetadata) -> String {
    let manual = Harness::ALL
        .iter()
        .filter(|harness| metadata.invocation.status(**harness) == InvocationStatus::ManualOnly)
        .map(|harness| harness.display_name())
        .collect::<Vec<_>>();
    let unknown = Harness::ALL
        .iter()
        .filter(|harness| metadata.invocation.status(**harness) == InvocationStatus::Unknown)
        .map(|harness| harness.display_name())
        .collect::<Vec<_>>();

    match (manual.is_empty(), unknown.is_empty()) {
        (true, true) => "—".to_string(),
        (false, true) => manual.join(", "),
        (true, false) => format!("unknown: {}", unknown.join(", ")),
        (false, false) => format!("{}; unknown: {}", manual.join(", "), unknown.join(", ")),
    }
}

fn installed_metadata(context: &ScopeContext, skill_id: &SkillId) -> SkillMetadata {
    let metadata = read_skill_metadata(&context.store.skill_dir(skill_id));
    for diagnostic in &metadata.diagnostics {
        eprintln!(
            "warning: could not fully read metadata for {skill_id} at {}: {}",
            display_user_path(diagnostic.path.as_str()),
            diagnostic.message
        );
    }
    metadata
}

fn lock_with_metadata_json(context: &ScopeContext, lock: &LockFile) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(lock)?;
    let skills = value
        .get_mut("skills")
        .and_then(serde_json::Value::as_object_mut)
        .expect("serialized lockfile skills must be an object");
    for skill_id in lock.skills.keys() {
        let metadata = installed_metadata(context, skill_id);
        skills
            .get_mut(skill_id.as_str())
            .and_then(serde_json::Value::as_object_mut)
            .expect("serialized skill record must be an object")
            .insert("metadata".to_string(), serde_json::to_value(metadata)?);
    }
    Ok(value)
}

fn display_user_path(value: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return value.to_string();
    };
    if home.is_empty() || value.starts_with('~') {
        return value.to_string();
    }
    let home = std::fs::canonicalize(&home)
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or(home);
    if value == home {
        return "~".to_string();
    }
    if let Some(rest) = value.strip_prefix(&format!("{home}/")) {
        return format!("~/{rest}");
    }
    value.to_string()
}

fn update(
    context: &ScopeContext,
    skills: &[String],
    requested_action: Option<ChangeAction>,
    json_output: bool,
) -> Result<()> {
    let mut lock = LockFile::read(&context.paths.lock_file())?;
    let reconcile_bundle = (skills.iter().any(|skill| skill == SELF_BUNDLE_ID)
        && lock.bundles.contains_key(SELF_BUNDLE_ID))
        || (skills.is_empty() && lock.bundles.contains_key(SELF_BUNDLE_ID));
    let mut selected = selected_skill_ids(context, &lock, skills, "update")?;
    let refresh_bundle_receipt = !reconcile_bundle
        && lock.bundles.contains_key(SELF_BUNDLE_ID)
        && selected.iter().any(|skill_id| {
            lock.skills.get(skill_id).is_some_and(|record| {
                record.source.kind == SourceKind::Bundled && record.source.path == SELF_BUNDLE_ID
            })
        });
    if reconcile_bundle {
        let current_members = self_bundle_skills()
            .iter()
            .map(|skill| SkillId::parse(skill.id))
            .collect::<yasm_core::Result<BTreeSet<_>>>()?;
        selected.retain(|skill_id| {
            lock.skills.get(skill_id).is_none_or(|record| {
                record.source.kind != SourceKind::Bundled
                    || record.source.path != SELF_BUNDLE_ID
                    || current_members.contains(skill_id)
            })
        });
    }
    let selected_count = selected.len();
    let mut candidates = Vec::new();
    let mut revision_recorded = Vec::new();
    let mut unchanged = Vec::new();
    let mut failed = Vec::new();
    let mut not_checked = Vec::new();
    let progress = progress::Progress::start(
        format!("Checking {selected_count} installed skill(s) ..."),
        json_output,
    );

    let mut selected_records = Vec::new();
    let mut unique_sources = BTreeMap::new();
    for skill_id in selected {
        let Some(record) = lock.skills.get(&skill_id).cloned() else {
            continue;
        };
        if record.source.kind == SourceKind::Owned {
            not_checked.push(UpdateNotChecked {
                skill: skill_id.to_string(),
                reason: "locally owned; no upstream".to_string(),
            });
            continue;
        }
        let key = SourceCheckoutKey::from(&record.source);
        unique_sources
            .entry(key.clone())
            .or_insert_with(|| record.source.clone());
        selected_records.push((skill_id, record, key));
    }

    let mut fetched_checkouts = BTreeMap::new();
    for (key, source) in unique_sources {
        progress.set_message(format!("Fetching {} ...", source.path));
        let checkout = fetch_checkout_cached(&source, &context.paths.cache_dir.join("sources"))
            .map_err(|error| error.to_string());
        fetched_checkouts.insert(key, checkout);
    }

    for (skill_id, record, key) in selected_records {
        let result = match fetched_checkouts.get(&key) {
            Some(Ok(checkout)) => resolve_fetched_source(&record.source, checkout)
                .map_err(anyhow::Error::from)
                .and_then(|fetched| {
                    check_update_candidate(context, &progress, &skill_id, record, fetched)
                }),
            Some(Err(error)) => Err(anyhow::anyhow!(error.clone())),
            None => Err(anyhow::anyhow!(
                "source checkout was not prepared for skill `{skill_id}`"
            )),
        };
        match result {
            Ok(Some(candidate)) if !candidate.diff.changed => {
                apply_update_candidate(context, &mut lock, candidate, &[], json_output)?;
                revision_recorded.push(skill_id.to_string());
            }
            Ok(Some(candidate)) => candidates.push(candidate),
            Ok(None) => unchanged.push(skill_id.to_string()),
            Err(error) => record_update_check_failure(
                &progress,
                &mut failed,
                &skill_id,
                format!("{error:#}"),
                json_output,
            ),
        }
    }
    progress.finish_and_clear();

    if !json_output {
        for skipped in &not_checked {
            println!("skipped {} ({})", skipped.skill, skipped.reason);
        }
    }

    let candidates = if candidates.is_empty() {
        candidates
    } else {
        select_update_candidates(candidates, !skills.is_empty())?
    };
    let mut updated = Vec::new();
    let mut skipped = Vec::new();

    if !candidates.is_empty() {
        let action = match requested_action {
            Some(action) => action,
            None => select_update_action()?,
        };
        for candidate in candidates {
            if !apply_update_action(&candidate, action, json_output)? {
                skipped.push(candidate.skill_id.to_string());
                if !json_output {
                    println!("skipped {}", candidate.skill_id);
                }
                continue;
            }

            let updated_skill_id = candidate.skill_id.to_string();
            apply_update_candidate(context, &mut lock, candidate, &[], json_output)?;
            updated.push(updated_skill_id.clone());
            if !json_output {
                println!("updated {updated_skill_id}");
            }
        }
    }

    let bundle_changes = if reconcile_bundle || refresh_bundle_receipt {
        let source = SourceInput::Bundled.into_spec()?;
        let fetched = match fetched_checkouts.get(&SourceCheckoutKey::from(&source)) {
            Some(Ok(checkout)) => resolve_fetched_source(&source, checkout)?,
            _ => fetch_source_cached(&source, &context.paths.cache_dir.join("sources"))?,
        };
        let discovery = discover_skills_with_diagnostics(&fetched.root)?;
        let bundle = SelfBundle::from_skills(&discovery.skills)?;
        if reconcile_bundle {
            reconcile_self_bundle(
                context,
                &mut lock,
                &bundle,
                &source,
                requested_action,
                json_output,
            )?
        } else {
            refresh_self_bundle_receipt_if_converged(context, &mut lock, &bundle)?;
            BundleReconcileResult::default()
        }
    } else {
        BundleReconcileResult::default()
    };
    skipped.extend(bundle_changes.skipped);
    if !json_output
        && failed.is_empty()
        && updated.is_empty()
        && bundle_changes.added.is_empty()
        && bundle_changes.retired.is_empty()
        && skipped.is_empty()
    {
        println!("no changes");
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "updated": updated,
                "revision_recorded": revision_recorded,
                "added": bundle_changes.added,
                "retired": bundle_changes.retired,
                "skipped": skipped,
                "unchanged": unchanged,
                "not_checked": not_checked.iter().map(|skipped| json!({
                    "skill": skipped.skill,
                    "reason": skipped.reason,
                })).collect::<Vec<_>>(),
                "failed": failed.iter().map(|failure| json!({
                    "skill": failure.skill,
                    "error": failure.error,
                })).collect::<Vec<_>>(),
            }))?
        );
    }
    if !failed.is_empty() {
        anyhow::bail!("failed to check {} skill(s) for updates", failed.len());
    }
    Ok(())
}

#[derive(Default)]
struct BundleReconcileResult {
    added: Vec<String>,
    retired: Vec<String>,
    skipped: Vec<String>,
}

fn reconcile_self_bundle(
    context: &ScopeContext,
    lock: &mut LockFile,
    bundle: &SelfBundle<'_>,
    source: &SourceSpec,
    requested_action: Option<ChangeAction>,
    json_output: bool,
) -> Result<BundleReconcileResult> {
    let Some(mut previous) = lock.bundles.get(SELF_BUNDLE_ID).cloned() else {
        anyhow::bail!("the `self` bundle is not acquired; run `yasm add self --action apply`");
    };
    previous.member_enabled = bundle_member_enabled(lock, Some(&previous));
    let current_ids = bundle.members();
    let acquired = acquired_self_member_ids(lock);
    let current_exclusions = previous.excluded.clone();
    let additions = current_ids
        .iter()
        .filter(|id| !current_exclusions.contains(*id) && !acquired.contains(*id))
        .cloned()
        .collect::<BTreeSet<_>>();
    let retirements = acquired
        .difference(&current_ids)
        .cloned()
        .collect::<BTreeSet<_>>();

    for skill_id in &additions {
        if let Some(existing) = lock.skills.get(skill_id) {
            anyhow::bail!(
                "cannot add bundled skill `{skill_id}` because that ID is already acquired from {}; remove it first",
                display_source_root(existing)
            );
        }
        if context.store.skill_dir(skill_id).exists() {
            anyhow::bail!(
                "cannot add bundled skill `{skill_id}` because the store path already exists without a lock record"
            );
        }
        let enabled = bundle_member_agents(&previous, skill_id);
        let agents = enabled
            .iter()
            .map(|id| context.registry.get(id.as_str()))
            .collect::<yasm_core::Result<Vec<_>>>()?;
        add_replacement_targets(context, &agents, skill_id, false)?;
    }

    let mut result = BundleReconcileResult::default();
    if !additions.is_empty() || !retirements.is_empty() {
        let action = match requested_action {
            Some(action) => action,
            None => select_update_action()?,
        };
        let apply = match action {
            ChangeAction::Apply => true,
            ChangeAction::Skip => false,
            ChangeAction::Review => {
                for skill_id in &additions {
                    if json_output {
                        eprintln!("bundle member to add: {skill_id}");
                    } else {
                        println!("bundle member to add: {skill_id}");
                    }
                }
                for skill_id in &retirements {
                    let modified = lock
                        .skills
                        .get(skill_id)
                        .and_then(|record| {
                            digest_skill_tree(&context.store.skill_dir(skill_id))
                                .ok()
                                .map(|digest| digest != record.digest)
                        })
                        .unwrap_or(false);
                    let suffix = if modified { " (locally modified)" } else { "" };
                    if json_output {
                        eprintln!("bundle member to retire: {skill_id}{suffix}");
                    } else {
                        println!("bundle member to retire: {skill_id}{suffix}");
                    }
                }
                interactive::ask_confirm(
                    "bundle update confirmation",
                    "Apply bundled membership changes?",
                    false,
                    "pass --action apply to reconcile without prompting, or --action skip to skip",
                )?
            }
        };
        if !apply {
            result.skipped.extend(
                additions
                    .iter()
                    .chain(&retirements)
                    .map(ToString::to_string),
            );
            return Ok(result);
        }

        for skill_id in &additions {
            let skill = bundle.skill(skill_id).ok_or_else(|| {
                anyhow::anyhow!("bundle addition `{skill_id}` is missing from the current bundle")
            })?;
            let enabled = bundle_member_agents(&previous, skill_id);
            let agents = enabled
                .iter()
                .map(|id| context.registry.get(id.as_str()))
                .collect::<yasm_core::Result<Vec<_>>>()?;
            context.lifecycle().acquire(
                lock,
                skill_id,
                skill.name.clone(),
                source.clone(),
                None,
                skill.skill_path.clone(),
                &skill.directory,
            )?;
            if !agents.is_empty() {
                context.lifecycle().enable(lock, skill_id, &agents, false)?;
            }
            result.added.push(skill_id.to_string());
            if !json_output {
                println!("added bundled skill {skill_id}");
            }
        }

        for skill_id in &retirements {
            context
                .lifecycle()
                .disable_all_then_remove(lock, skill_id)?;
            result.retired.push(skill_id.to_string());
            if !json_output {
                println!("retired bundled skill {skill_id}");
            }
        }
    }

    if bundle.is_converged(context, lock, &current_exclusions)? {
        bundle.write_receipt(
            context,
            lock,
            Some(&previous),
            current_exclusions,
            previous.enabled.clone(),
        )?;
    }
    Ok(result)
}

fn refresh_self_bundle_receipt_if_converged(
    context: &ScopeContext,
    lock: &mut LockFile,
    bundle: &SelfBundle<'_>,
) -> Result<()> {
    let Some(previous) = lock.bundles.get(SELF_BUNDLE_ID).cloned() else {
        return Ok(());
    };
    if bundle.is_converged(context, lock, &previous.excluded)? {
        bundle.write_receipt(
            context,
            lock,
            Some(&previous),
            previous.excluded.clone(),
            previous.enabled.clone(),
        )?;
    }
    Ok(())
}

fn bundle_member_agents(previous: &LockedBundleRecord, new_id: &SkillId) -> BTreeSet<AgentId> {
    previous
        .member_enabled
        .get(new_id)
        .unwrap_or(&previous.enabled)
        .clone()
}

fn check_update_candidate(
    context: &ScopeContext,
    progress: &progress::Progress,
    skill_id: &SkillId,
    record: LockedSkillRecord,
    fetched: FetchedSource,
) -> Result<Option<UpdateCandidate>> {
    progress.set_message(format!("Reading {skill_id} ..."));
    let skill_file = fetched.root.join(record.skill_path.as_str());
    if !skill_file.is_file() {
        anyhow::bail!(
            "recorded path `{}` for skill `{skill_id}` was not found in the fetched source; re-add the skill if it moved",
            record.skill_path.as_str()
        );
    }
    let source_root = std::fs::canonicalize(&fetched.root)
        .with_context(|| format!("failed to resolve fetched source root {}", fetched.root))?;
    let resolved_skill_file = std::fs::canonicalize(&skill_file)
        .with_context(|| format!("failed to resolve recorded skill path {skill_file}"))?;
    if !resolved_skill_file.starts_with(&source_root) {
        anyhow::bail!(
            "recorded path `{}` for skill `{skill_id}` resolves outside the fetched source",
            record.skill_path.as_str()
        );
    }
    let selected_skill = parse_skill_file(&fetched.root, &skill_file).with_context(|| {
        format!(
            "could not parse skill `{skill_id}` at recorded path `{}`",
            record.skill_path.as_str()
        )
    })?;
    let installed_path = context.store.skill_dir(skill_id);
    progress.set_message(format!("Comparing {skill_id} ..."));
    let diff = skill_source_directory_diff(
        skill_id,
        &installed_path,
        &selected_skill.directory,
        record.source.kind.is_git(),
    )?;
    if !diff.changed && (fetched.resolved.is_none() || record.resolved == fetched.resolved) {
        return Ok(None);
    }

    Ok(Some(UpdateCandidate {
        skill_id: skill_id.clone(),
        record,
        fetched,
        selected_skill,
        diff,
    }))
}

fn record_update_check_failure(
    progress: &progress::Progress,
    failed: &mut Vec<UpdateCheckFailure>,
    skill_id: &SkillId,
    error: String,
    json_output: bool,
) {
    let failure = UpdateCheckFailure {
        skill: skill_id.to_string(),
        error,
    };
    if !json_output {
        progress.warn(format!(
            "failed to check {} for updates: {}",
            failure.skill, failure.error
        ));
    }
    failed.push(failure);
}

fn apply_update_action(
    candidate: &UpdateCandidate,
    action: ChangeAction,
    json_output: bool,
) -> Result<bool> {
    match action {
        // The installed files already match upstream, so the fetched commit can be
        // stored without a separate confirmation.
        ChangeAction::Review if !candidate.diff.changed => Ok(true),
        ChangeAction::Review => {
            print_skill_diff(&candidate.skill_id, &candidate.diff, json_output);
            interactive::ask_confirm(
                "update confirmation",
                &format!("Apply update for {}?", candidate.skill_id),
                false,
                "pass --action apply to update without prompting, or --action skip to skip",
            )
        }
        ChangeAction::Apply => Ok(true),
        ChangeAction::Skip => Ok(false),
    }
}

fn apply_update_candidate(
    context: &ScopeContext,
    lock: &mut LockFile,
    candidate: UpdateCandidate,
    agents: &[&Agent],
    quiet_progress: bool,
) -> Result<()> {
    if !candidate.diff.changed {
        let mut record = candidate.record;
        record.resolved = candidate.fetched.resolved;
        record.digest = digest_skill_tree(&context.store.skill_dir(&candidate.skill_id))?;
        lock.skills.insert(candidate.skill_id, record);
        lock.write(&context.paths.lock_file())?;
        return Ok(());
    }
    let install_progress = progress::Progress::start(
        format!("Refreshing {} ...", candidate.skill_id),
        quiet_progress,
    );
    let lifecycle = context.lifecycle();
    let record = candidate.record;
    lifecycle.acquire(
        lock,
        &candidate.skill_id,
        candidate.selected_skill.name,
        record.source,
        candidate.fetched.resolved,
        candidate.selected_skill.skill_path,
        &candidate.selected_skill.directory,
    )?;
    if !agents.is_empty() {
        lifecycle.enable(lock, &candidate.skill_id, agents, false)?;
    }
    install_progress.finish_and_clear();
    Ok(())
}

fn print_unchanged_add_updates(skill_ids: &[SkillId]) {
    match skill_ids {
        [] => {}
        [_] => println!("no changes"),
        _ => println!(
            "no changes for {}",
            skill_ids
                .iter()
                .map(SkillId::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn select_add_managed_update_candidates(
    candidates: Vec<PreparedManagedAddCandidate<'_>>,
    requested_action: Option<ChangeAction>,
) -> Result<Vec<PreparedManagedAddCandidate<'_>>> {
    if candidates.is_empty() {
        return Ok(candidates);
    }
    if requested_action.is_some() {
        return Ok(candidates);
    }

    let labels = candidates
        .iter()
        .map(|candidate| candidate.managed.skill_id.to_string())
        .collect::<Vec<_>>();
    let defaults = vec![true; labels.len()];
    let selections = interactive::ask_multiselect(
        "managed skill updates",
        "Some selected skills are already installed. Select the ones to update",
        &labels,
        &defaults,
        "pass --action apply to update without prompting, or --action skip to skip",
    )?;
    let selected = selections.into_iter().collect::<BTreeSet<_>>();
    Ok(candidates
        .into_iter()
        .enumerate()
        .filter_map(|(idx, candidate)| {
            if selected.contains(&idx) {
                Some(candidate)
            } else {
                println!("skipped {}", candidate.managed.skill_id);
                None
            }
        })
        .collect())
}

struct AddManagedCandidate<'a> {
    skill_id: SkillId,
    skill: &'a DiscoveredSkill,
    existing_record: LockedSkillRecord,
    same_upstream: bool,
    agents: Vec<&'a Agent>,
}

struct PreparedManagedAddCandidate<'a> {
    managed: AddManagedCandidate<'a>,
    diff: DirectoryDiff,
}

struct AddInstallCandidate<'a> {
    skill_id: SkillId,
    skill: &'a DiscoveredSkill,
    replacements: BTreeSet<Utf8PathBuf>,
    agents: Vec<&'a Agent>,
}

struct PreparedAddUpdate<'a> {
    candidate: UpdateCandidate,
    agents: Vec<&'a Agent>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SourceCheckoutKey {
    Bundled {
        path: String,
    },
    Owned,
    Local {
        path: String,
    },
    Git {
        path: String,
        git_ref: Option<String>,
    },
}

impl From<&SourceSpec> for SourceCheckoutKey {
    fn from(source: &SourceSpec) -> Self {
        match source.kind {
            SourceKind::Bundled => Self::Bundled {
                path: source.path.clone(),
            },
            SourceKind::Owned => Self::Owned,
            SourceKind::Local => Self::Local {
                path: source.path.clone(),
            },
            SourceKind::Github | SourceKind::Git => Self::Git {
                path: source.path.clone(),
                git_ref: source
                    .r#ref
                    .as_ref()
                    .map(|git_ref| git_ref.as_str().to_string()),
            },
        }
    }
}

struct UpdateCandidate {
    skill_id: SkillId,
    record: LockedSkillRecord,
    fetched: FetchedSource,
    selected_skill: DiscoveredSkill,
    diff: DirectoryDiff,
}

struct UpdateCheckFailure {
    skill: String,
    error: String,
}

struct UpdateNotChecked {
    skill: String,
    reason: String,
}

struct DirectoryDiff {
    changed: bool,
    output: String,
    terminal_output: Option<String>,
}

fn add_replacement_targets(
    context: &ScopeContext,
    agents: &[&Agent],
    skill_id: &SkillId,
    replace: bool,
) -> Result<BTreeSet<Utf8PathBuf>> {
    let mut replacements = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for agent in agents {
        let resolved_link = yasm_core::resolved_agent_skill_link(agent, skill_id)?;
        if !seen.insert(resolved_link.clone()) {
            continue;
        }
        let link = agent.skill_link(skill_id);
        match std::fs::symlink_metadata(&link) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let existing = std::fs::read_link(&link)
                    .with_context(|| format!("failed to read symlink {link}"))?;
                let existing = Utf8PathBuf::from_path_buf(existing).map_err(|path| {
                    anyhow::anyhow!("non-UTF-8 symlink target: {}", path.display())
                })?;
                let expected = context.lifecycle().skill_link_target(agent, skill_id)?;
                if existing == expected {
                    continue;
                }
                anyhow::bail!("agent skill path already exists as a symlink: {link} -> {existing}");
            }
            Ok(metadata) if metadata.is_dir() => {
                if replace || std::io::stdin().is_terminal() {
                    replacements.insert(resolved_link);
                } else {
                    anyhow::bail!(
                        "agent skill path already exists and is not managed by yasm: {link}; pass --replace to replace existing unmanaged skill directories"
                    );
                }
            }
            Ok(_) => {
                anyhow::bail!("agent skill path already exists and is not managed by yasm: {link}");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(source).with_context(|| format!("failed to inspect {link}")),
        }
    }
    Ok(replacements)
}

fn select_add_action(candidates: &[AddInstallCandidate<'_>]) -> Result<ChangeAction> {
    let has_replacements = candidates
        .iter()
        .any(|candidate| !candidate.replacements.is_empty());
    if !has_replacements {
        return select_change_action(
            "add action",
            &format!(
                "How do you want to install selected skills ({})?",
                add_candidate_skill_ids(candidates)
            ),
            "Review changes",
            "Install without review",
            "pass --action apply to install without prompting, or --action skip to skip",
        );
    }

    for candidate in candidates {
        if !candidate.replacements.is_empty() {
            print_existing_skill_message(
                &candidate.skill_id,
                &replacement_agent_ids(
                    &candidate.agents,
                    &candidate.skill_id,
                    &candidate.replacements,
                )?,
            );
        }
    }

    let all_replacements = candidates
        .iter()
        .all(|candidate| !candidate.replacements.is_empty());
    let prompt = if all_replacements {
        format!(
            "How do you want to overwrite selected skills ({})?",
            add_candidate_skill_ids(candidates)
        )
    } else {
        format!(
            "How do you want to install or overwrite selected skills ({})?",
            add_candidate_skill_ids(candidates)
        )
    };
    let apply_label = if all_replacements {
        "Overwrite without review"
    } else {
        "Install or overwrite without review"
    };

    select_change_action(
        "overwrite action",
        &prompt,
        "Review changes",
        apply_label,
        "pass --action apply --replace to overwrite without prompting, or --action skip to skip",
    )
}

fn add_candidate_skill_ids(candidates: &[AddInstallCandidate<'_>]) -> String {
    candidates
        .iter()
        .map(|candidate| candidate.skill_id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn add_confirmation_prompt(skill_id: &SkillId, overwriting: bool) -> String {
    if overwriting {
        format!("Overwrite {skill_id}?")
    } else {
        format!("Install {skill_id}?")
    }
}

fn replacement_agent_ids(
    agents: &[&Agent],
    skill_id: &SkillId,
    replacements: &BTreeSet<Utf8PathBuf>,
) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for agent in agents {
        let resolved_link = yasm_core::resolved_agent_skill_link(agent, skill_id)?;
        if replacements.contains(&resolved_link) {
            ids.push(agent.id.as_str().to_string());
        }
    }
    Ok(ids)
}

fn print_existing_skill_message(skill_id: &SkillId, agent_ids: &[String]) {
    let targets = agent_ids.join(", ");
    if agent_ids.len() == 1 {
        println!("ℹ Existing skill folder for {skill_id}: {targets}");
    } else {
        println!("ℹ Existing skill folders for {skill_id}: {targets}");
    }
}

fn add_candidate_diff(
    skill_id: &SkillId,
    skill: &DiscoveredSkill,
    replacements: &BTreeSet<Utf8PathBuf>,
    candidate_is_git_checkout: bool,
) -> Result<DirectoryDiff> {
    if let Some(existing) = replacements.first() {
        return skill_source_directory_diff_with_labels(
            skill_id,
            existing,
            "existing",
            &skill.directory,
            "candidate",
            candidate_is_git_checkout,
        );
    }

    let temp = tempdir()?;
    let empty = Utf8PathBuf::from_path_buf(temp.path().join("empty"))
        .map_err(|path| anyhow::anyhow!("non-UTF-8 temp path: {}", path.display()))?;
    std::fs::create_dir_all(&empty).context("failed to create empty diff directory")?;
    skill_source_directory_diff_with_labels(
        skill_id,
        &empty,
        "empty",
        &skill.directory,
        "candidate",
        candidate_is_git_checkout,
    )
}

fn print_add_diff(skill_id: &SkillId, diff: &DirectoryDiff) {
    if diff.output.trim().is_empty() && !diff.changed {
        page_or_print(&format!(
            "no content changes between existing copy and fetched candidate for {skill_id}\n"
        ));
        return;
    }
    if diff.output.trim().is_empty() {
        page_or_print(&format!(
            "fetched candidate contents changed for {skill_id}, but no textual diff was produced\n"
        ));
        return;
    }
    print_skill_diff(skill_id, diff, false);
}

fn select_update_candidates(
    candidates: Vec<UpdateCandidate>,
    explicit_skills: bool,
) -> Result<Vec<UpdateCandidate>> {
    if explicit_skills {
        return Ok(candidates);
    }

    let labels = candidates
        .iter()
        .map(|candidate| candidate.skill_id.to_string())
        .collect::<Vec<_>>();
    let defaults = vec![true; labels.len()];
    let selections = interactive::ask_multiselect(
        "skills",
        "Select skills to update",
        &labels,
        &defaults,
        "pass one or more skill IDs explicitly",
    )?;
    if selections.is_empty() {
        anyhow::bail!("no skills selected");
    }

    let selected = selections.into_iter().collect::<BTreeSet<_>>();
    Ok(candidates
        .into_iter()
        .enumerate()
        .filter_map(|(idx, candidate)| selected.contains(&idx).then_some(candidate))
        .collect())
}

fn select_change_action(
    field: &str,
    prompt: &str,
    review_label: &str,
    apply_label: &str,
    hint: &str,
) -> Result<ChangeAction> {
    let labels = vec![
        review_label.to_string(),
        apply_label.to_string(),
        "Cancel".to_string(),
    ];
    let selected = interactive::ask_select(field, prompt, &labels, hint)?;
    match selected {
        0 => Ok(ChangeAction::Review),
        1 => Ok(ChangeAction::Apply),
        _ => Ok(ChangeAction::Skip),
    }
}

fn select_update_action() -> Result<ChangeAction> {
    select_change_action(
        "update action",
        "How do you want to update selected skills?",
        "Review changes",
        "Update without review",
        "pass --action <review|apply|skip>",
    )
}

fn print_skill_diff(skill_id: &SkillId, diff: &DirectoryDiff, stderr: bool) {
    let terminal = if stderr {
        std::io::stderr().is_terminal()
    } else {
        std::io::stdout().is_terminal()
    };
    let rendered = if terminal {
        diff.terminal_output.as_deref().unwrap_or(&diff.output)
    } else {
        &diff.output
    };
    let output = if rendered.trim().is_empty() {
        format!("installed copy differs from fetched candidate for {skill_id}\n")
    } else {
        format!("{}\n", rendered.trim_end())
    };
    if stderr {
        eprint!("{output}");
    } else {
        page_or_print(&output);
    }
}

fn skill_directory_diff(
    skill_id: &SkillId,
    installed: &Utf8Path,
    candidate: &Utf8Path,
) -> Result<DirectoryDiff> {
    skill_source_directory_diff_with_labels(
        skill_id,
        installed,
        "installed",
        candidate,
        "candidate",
        false,
    )
}

fn skill_source_directory_diff(
    skill_id: &SkillId,
    installed: &Utf8Path,
    candidate: &Utf8Path,
    candidate_is_git_checkout: bool,
) -> Result<DirectoryDiff> {
    skill_source_directory_diff_with_labels(
        skill_id,
        installed,
        "installed",
        candidate,
        "candidate",
        candidate_is_git_checkout,
    )
}

fn skill_source_directory_diff_with_labels(
    skill_id: &SkillId,
    installed: &Utf8Path,
    installed_label: &str,
    candidate: &Utf8Path,
    candidate_label: &str,
    candidate_is_git_checkout: bool,
) -> Result<DirectoryDiff> {
    if std::fs::symlink_metadata(installed)
        .map(|metadata| !metadata.is_dir())
        .unwrap_or(false)
    {
        return Ok(DirectoryDiff {
            changed: true,
            output: format!("{installed_label} copy is not a directory: {installed}"),
            terminal_output: None,
        });
    }
    if !installed.exists() {
        return Ok(DirectoryDiff {
            changed: true,
            output: format!("{installed_label} copy is missing: {installed}"),
            terminal_output: None,
        });
    }

    let installed_files = collect_diff_files(installed, false)?;
    let candidate_files = collect_diff_files(candidate, candidate_is_git_checkout)?;
    let mut paths = installed_files
        .union(&candidate_files)
        .cloned()
        .collect::<Vec<_>>();
    paths.sort_by(diff_path_cmp);

    let width = (std::io::stdout().is_terminal() || std::io::stderr().is_terminal())
        .then(terminal_diff::terminal_width);
    let mut terminal_output = width.map(|_| String::new());
    let mut changed = false;
    let mut output = String::new();
    for path in paths {
        let installed_path = installed.join(&path);
        let candidate_path = candidate.join(&path);
        let installed_entry = diff_entry(&installed_path)?;
        let candidate_entry = if candidate_files.contains(&path) {
            diff_entry(&candidate_path)?
        } else {
            DiffEntry::Missing
        };
        if installed_entry == candidate_entry {
            continue;
        }
        changed = true;
        let installed_name = format!("{installed_label}/{}/{}", skill_id.as_str(), path);
        let candidate_name = format!("{candidate_label}/{}/{}", skill_id.as_str(), path);
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!("diff -- {installed_name} {candidate_name}\n"));
        let (plain, formatted) = format_entry_diff(
            &installed_name,
            &candidate_name,
            &installed_entry,
            &candidate_entry,
            width,
        );
        output.push_str(&plain);
        if let (Some(output), Some(formatted)) = (&mut terminal_output, formatted) {
            output.push_str(&formatted);
        }
    }

    if !changed {
        let candidate_digest = if candidate_is_git_checkout {
            digest_skill_source_tree(candidate)?
        } else {
            digest_skill_tree(candidate)?
        };
        changed = digest_skill_tree(installed)? != candidate_digest;
    }

    Ok(DirectoryDiff {
        changed,
        output,
        terminal_output,
    })
}

fn collect_diff_files(
    root: &Utf8Path,
    exclude_git_metadata: bool,
) -> Result<BTreeSet<Utf8PathBuf>> {
    let mut files = BTreeSet::new();
    collect_diff_files_inner(root, root, &mut files, exclude_git_metadata)?;
    Ok(files)
}

fn collect_diff_files_inner(
    root: &Utf8Path,
    current: &Utf8Path,
    files: &mut BTreeSet<Utf8PathBuf>,
    exclude_git_metadata: bool,
) -> Result<()> {
    let mut entries = std::fs::read_dir(current)
        .with_context(|| format!("failed to read diff directory {current}"))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read diff directory {current}"))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        if exclude_git_metadata && entry.file_name() == ".git" {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| anyhow::anyhow!("non-UTF-8 diff path: {}", path.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect diff path {path}"))?;
        if file_type.is_dir() {
            collect_diff_files_inner(root, &path, files, exclude_git_metadata)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            files.insert(path.strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(())
}

fn diff_path_cmp(left: &Utf8PathBuf, right: &Utf8PathBuf) -> std::cmp::Ordering {
    diff_path_rank(left)
        .cmp(&diff_path_rank(right))
        .then_with(|| left.cmp(right))
}

fn diff_path_rank(path: &Utf8Path) -> u8 {
    match path.file_name() {
        Some("SKILL.md") => 0,
        Some(name) if name.ends_with(".md") => 1,
        _ => 2,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DiffEntry {
    Missing,
    Text(String),
    Binary(Vec<u8>),
    Symlink(String),
}

fn diff_entry(path: &Utf8Path) -> Result<DiffEntry> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DiffEntry::Missing)
        }
        Err(source) => return Err(source).with_context(|| format!("failed to inspect {path}")),
    };
    if metadata.file_type().is_symlink() {
        let target =
            std::fs::read_link(path).with_context(|| format!("failed to read symlink {path}"))?;
        return Ok(DiffEntry::Symlink(format!("{}\n", target.display())));
    }
    let bytes = std::fs::read(path).with_context(|| format!("failed to read {path}"))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(DiffEntry::Text(text)),
        Err(error) => Ok(DiffEntry::Binary(error.into_bytes())),
    }
}

fn format_entry_diff(
    installed_name: &str,
    candidate_name: &str,
    installed: &DiffEntry,
    candidate: &DiffEntry,
    terminal_width: Option<usize>,
) -> (String, Option<String>) {
    match (installed, candidate) {
        (DiffEntry::Binary(_), _) | (_, DiffEntry::Binary(_)) => {
            let output = format!("Binary files {installed_name} and {candidate_name} differ\n");
            let terminal = terminal_width.map(|_| output.clone());
            (output, terminal)
        }
        _ => {
            let old = diff_entry_text(installed);
            let new = diff_entry_text(candidate);
            let diff = TextDiff::from_lines(&old, &new);
            let terminal = terminal_width
                .map(|width| terminal_diff::render(&diff, candidate_name, &old, &new, width));
            let plain = diff
                .unified_diff()
                .header(installed_name, candidate_name)
                .to_string();
            (plain, terminal)
        }
    }
}

fn diff_entry_text(entry: &DiffEntry) -> String {
    match entry {
        DiffEntry::Missing | DiffEntry::Binary(_) => String::new(),
        DiffEntry::Text(text) | DiffEntry::Symlink(text) => text.clone(),
    }
}

fn page_or_print(output: &str) {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        print!("{output}");
        return;
    }

    let pager = std::env::var("PAGER")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PAGER_COMMAND.to_string());

    let mut command = ProcessCommand::new("sh");
    command.arg("-c").arg(&pager).stdin(Stdio::piped());
    if let Some(less) = default_less_env(&pager, std::env::var_os("LESS").is_some()) {
        command.env("LESS", less);
    }

    match command.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(error) = stdin.write_all(output.as_bytes()) {
                    drop(stdin);
                    let _ = child.wait();
                    if pager_write_error_should_fallback(&error) {
                        print!("{output}");
                    }
                    return;
                }
            }
            if child.wait().is_err() {
                print!("{output}");
            }
        }
        Err(_) => print!("{output}"),
    }
}

fn pager_write_error_should_fallback(error: &std::io::Error) -> bool {
    error.kind() != std::io::ErrorKind::BrokenPipe
}

fn default_less_env(pager: &str, less_is_set: bool) -> Option<&'static str> {
    if less_is_set || !pager.starts_with("less") {
        return None;
    }
    Some(DEFAULT_LESS_ENV)
}

fn enable(
    context: &ScopeContext,
    skills: &[String],
    agent_filter: &[LinkTarget],
    replace: bool,
) -> Result<()> {
    let mut lock = LockFile::read(&context.paths.lock_file())?;
    let targets_self = skills.iter().any(|skill| skill == SELF_BUNDLE_ID)
        && lock.bundles.contains_key(SELF_BUNDLE_ID);
    let selected = select_locked_skill_ids(
        context,
        &lock,
        skills,
        false,
        "enable",
        "Select skills to enable",
        "pass one or more skill IDs explicitly",
    )?;
    let agents = require_agents(&context.registry, agent_filter)?;
    let lifecycle = context.lifecycle();
    let mut bundled_member_changed = false;
    for skill_id in selected {
        lifecycle.enable(&mut lock, &skill_id, &agents, replace)?;
        if let Some(enabled) = lock.skills.get(&skill_id).and_then(|record| {
            (record.source.kind == SourceKind::Bundled && record.source.path == SELF_BUNDLE_ID)
                .then(|| record.enabled.clone())
        }) {
            if let Some(bundle) = lock.bundles.get_mut(SELF_BUNDLE_ID) {
                bundle.member_enabled.insert(skill_id.clone(), enabled);
                bundled_member_changed = true;
            }
        }
        println!("enabled {skill_id} for {}", display_agent_ids(&agents));
    }
    if targets_self {
        if let Some(bundle) = lock.bundles.get_mut(SELF_BUNDLE_ID) {
            bundle.enabled.extend(agent_ids(&agents));
        }
    }
    if targets_self || bundled_member_changed {
        lock.write(&context.paths.lock_file())?;
    }
    Ok(())
}

fn disable(context: &ScopeContext, skills: &[String], agent_filter: &[LinkTarget]) -> Result<()> {
    let mut lock = LockFile::read(&context.paths.lock_file())?;
    let targets_self = skills.iter().any(|skill| skill == SELF_BUNDLE_ID)
        && lock.bundles.contains_key(SELF_BUNDLE_ID);
    let selected = select_locked_skill_ids(
        context,
        &lock,
        skills,
        false,
        "disable",
        "Select skills to disable",
        "pass one or more skill IDs explicitly",
    )?;
    let agents = require_agents(&context.registry, agent_filter)?;
    let lifecycle = context.lifecycle();
    let mut bundled_member_changed = false;
    for skill_id in selected {
        lifecycle.disable(&mut lock, &skill_id, &agents)?;
        if let Some(enabled) = lock.skills.get(&skill_id).and_then(|record| {
            (record.source.kind == SourceKind::Bundled && record.source.path == SELF_BUNDLE_ID)
                .then(|| record.enabled.clone())
        }) {
            if let Some(bundle) = lock.bundles.get_mut(SELF_BUNDLE_ID) {
                bundle.member_enabled.insert(skill_id.clone(), enabled);
                bundled_member_changed = true;
            }
        }
        println!("disabled {skill_id} for {}", display_agent_ids(&agents));
    }
    if targets_self {
        if let Some(bundle) = lock.bundles.get_mut(SELF_BUNDLE_ID) {
            for agent in &agents {
                bundle.enabled.remove(&agent.id);
            }
        }
    }
    if targets_self || bundled_member_changed {
        lock.write(&context.paths.lock_file())?;
    }
    Ok(())
}

fn status(context: &ScopeContext, json_output: bool) -> Result<()> {
    let lock = LockFile::read(&context.paths.lock_file())?;
    let lifecycle = context.lifecycle();
    let statuses = collect_status(&lifecycle, &lock, &context.store, &context.registry)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&statuses)?);
        return Ok(());
    }
    if statuses.is_empty() {
        if lock.bundles.contains_key(SELF_BUNDLE_ID) {
            println!("self: bundle present; no members acquired");
            print_self_bundle_freshness(&lock);
        } else {
            println!("no skills acquired");
        }
        return Ok(());
    }
    for skill in statuses {
        let store_health = match (skill.store_present, skill.digest_ok) {
            (false, _) => "store missing",
            (true, Some(true)) => "store present; digest ok",
            (true, Some(false)) => "store present; digest mismatch",
            (true, None) => "store present",
        };
        println!("{}: {store_health}", skill.skill_id);
        if skill.enabled.is_empty() {
            println!("  enabled: none");
        } else {
            println!("  enabled: {}", skill.enabled.join(", "));
        }
        for link in skill.links {
            let health = match link.health {
                LinkHealth::Present => "present".to_string(),
                LinkHealth::Missing => "missing".to_string(),
                LinkHealth::Foreign { target } => format!("foreign -> {target}"),
                LinkHealth::Broken { target } => format!("broken -> {target}"),
                LinkHealth::PlainFile => {
                    "plain file (Git core.symlinks=false? set core.symlinks true and re-checkout, or clone in WSL)".to_string()
                }
                LinkHealth::UnmanagedDirectory => "unmanaged directory".to_string(),
            };
            let inside = match link.inside_project_store {
                Some(true) => "; inside .yasm/skills",
                Some(false) => "; TARGET OUTSIDE .yasm/skills",
                None => "",
            };
            println!("  {}: {health}{inside}", link.agent);
        }
    }
    print_self_bundle_freshness(&lock);
    Ok(())
}

fn print_self_bundle_freshness(lock: &LockFile) {
    if let Some(bundle) = lock.bundles.get(SELF_BUNDLE_ID) {
        if bundle.digest != self_bundle_digest() {
            println!(
                "self: bundled content differs from the running executable; run `yasm update self --action apply`"
            );
        }
    }
}

fn doctor(context: &ScopeContext, repair: bool, replace: bool, json_output: bool) -> Result<()> {
    if !repair {
        anyhow::bail!("doctor is mutating; pass --repair to recreate missing yasm-owned links");
    }
    let mut lock = LockFile::read(&context.paths.lock_file())?;
    let lifecycle = context.lifecycle();
    let repaired = repair_owned_links(&lifecycle, &mut lock, replace)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&repaired)?);
        return Ok(());
    }
    if repaired.is_empty() {
        println!("no repairs needed");
        return Ok(());
    }
    for (skill_id, actions) in repaired {
        for action in actions {
            println!("{skill_id}: {action}");
        }
    }
    Ok(())
}

fn remove(context: &ScopeContext, skills: &[String], all: bool, json_output: bool) -> Result<()> {
    let mut lock = LockFile::read(&context.paths.lock_file())?;
    let removes_self = skills.iter().any(|skill| skill == SELF_BUNDLE_ID)
        && lock.bundles.contains_key(SELF_BUNDLE_ID);
    let selected = select_locked_skill_ids(
        context,
        &lock,
        skills,
        json_output,
        "remove",
        "Select skills to remove",
        "pass one or more skill names explicitly, or pass --all with explicit skills",
    )?;
    if selected.is_empty() {
        if removes_self {
            lock.bundles.remove(SELF_BUNDLE_ID);
            lock.write(&context.paths.lock_file())?;
            if !json_output {
                println!("removed {SELF_BUNDLE_ID}");
            }
        }
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "removed": if removes_self { vec![SELF_BUNDLE_ID] } else { Vec::new() },
                    "removed_links": [],
                }))?
            );
        }
        return Ok(());
    }
    let mut removed_skills = Vec::new();
    let mut removed_links = Vec::new();
    let lifecycle = context.lifecycle();
    for skill_id in selected {
        let bundled_member = lock.skills.get(&skill_id).is_some_and(|record| {
            record.source.kind == SourceKind::Bundled && record.source.path == SELF_BUNDLE_ID
        });
        if all {
            let enabled_agents = lock
                .skills
                .get(&skill_id)
                .map(|record| record.enabled.clone())
                .unwrap_or_default();
            lifecycle.disable_all_then_remove(&mut lock, &skill_id)?;
            for agent_id in &enabled_agents {
                if let Ok(agent) = context.registry.get(agent_id.as_str()) {
                    let displayed = agent.skill_link(&skill_id).to_string();
                    removed_links.push(displayed.clone());
                }
            }
            removed_skills.push(skill_id.to_string());
            if bundled_member && !removes_self {
                if let Some(bundle) = lock.bundles.get_mut(SELF_BUNDLE_ID) {
                    bundle.excluded.insert(skill_id.clone());
                    lock.write(&context.paths.lock_file())?;
                }
            }
            if !json_output {
                println!("removed {skill_id}");
            }
            continue;
        }

        match lifecycle.remove(&mut lock, &skill_id) {
            Ok(_) => {
                removed_skills.push(skill_id.to_string());
                if bundled_member && !removes_self {
                    if let Some(bundle) = lock.bundles.get_mut(SELF_BUNDLE_ID) {
                        bundle.excluded.insert(skill_id.clone());
                        lock.write(&context.paths.lock_file())?;
                    }
                }
                if !json_output {
                    println!("removed {skill_id}");
                }
            }
            Err(yasm_core::Error::SkillStillEnabled { skill, agents }) => {
                anyhow::bail!(
                    "skill `{skill}` is still enabled for {agents}; run `yasm disable {skill} --agent <agent>` or `yasm remove {skill} --all`"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }

    if removes_self {
        lock.bundles.remove(SELF_BUNDLE_ID);
        lock.write(&context.paths.lock_file())?;
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "removed": removed_skills,
                "removed_links": removed_links,
            }))?
        );
    }
    Ok(())
}

fn select_add_skills<'plan, 'skill>(
    plan: &'plan [add_plan::PlannedSkill<'skill>],
    selection: Option<&str>,
) -> Result<Vec<&'plan add_plan::PlannedSkill<'skill>>> {
    if let Some(selection) = selection {
        let matches = plan
            .iter()
            .filter(|planned| {
                planned.skill.name.as_str() == selection
                    || planned.skill.skill_path.as_str() == selection
            })
            .collect::<Vec<_>>();
        return match matches.as_slice() {
            [] => anyhow::bail!(
                "skill `{selection}` was not found; available skills: {}",
                if plan.is_empty() {
                    "none".to_string()
                } else {
                    plan.iter()
                        .map(|planned| planned.skill.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
            [skill] => Ok(vec![*skill]),
            _ => anyhow::bail!(
                "skill `{selection}` is ambiguous; pass --skill <repository-relative SKILL.md path>; matching paths: {}",
                matches
                    .iter()
                    .map(|planned| planned.skill.skill_path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    if plan.is_empty() {
        anyhow::bail!("no valid skills found in source");
    }
    let installed = plan
        .iter()
        .filter(|planned| planned.state == add_plan::AddSkillState::Installed)
        .collect::<Vec<_>>();
    if !installed.is_empty() {
        println!("Already installed from this source:");
        for planned in &installed {
            println!("✔ {}", display_discovered_skill(planned.skill));
        }
        println!();
    }
    let available = plan
        .iter()
        .filter(|planned| planned.state != add_plan::AddSkillState::Installed)
        .collect::<Vec<_>>();
    if available.is_empty() {
        return Ok(Vec::new());
    }
    // Preserve automatic selection only for a source containing a single skill.
    // Even one remaining skill in a larger source needs an explicit choice.
    if plan.len() == 1 {
        return Ok(vec![available[0]]);
    }
    let labels = available
        .iter()
        .map(|planned| {
            let label = display_discovered_skill(planned.skill);
            if planned.state == add_plan::AddSkillState::Conflict {
                format!("{label} (different installed source; requires --replace)")
            } else {
                label
            }
        })
        .collect::<Vec<_>>();
    let selections = interactive::ask_multiselect(
        "skills",
        "Select additional skills to install",
        &labels,
        &vec![false; labels.len()],
        "pass --skill <name> to choose a skill explicitly",
    )?;
    if selections.is_empty() {
        anyhow::bail!("no skills selected");
    }
    Ok(selections
        .into_iter()
        .filter_map(|index| available.get(index).copied())
        .collect())
}

fn selected_skill_ids(
    context: &ScopeContext,
    lock: &LockFile,
    requested: &[String],
    command: &str,
) -> Result<Vec<SkillId>> {
    if requested.is_empty() {
        return Ok(lock.skills.keys().cloned().collect());
    }

    let mut selected = BTreeSet::new();
    for id in requested {
        if id == SELF_BUNDLE_ID {
            let members = acquired_self_member_ids(lock);
            if members.is_empty() && !lock.bundles.contains_key(SELF_BUNDLE_ID) {
                if context.is_project() {
                    let global = LockFile::read(&YasmPaths::discover()?.lock_file())?;
                    if global.bundles.contains_key(SELF_BUNDLE_ID) {
                        anyhow::bail!(
                            "the `self` bundle is not installed in this project, but exists globally.\nRun `yasm {command} self --global` to {command} it globally."
                        );
                    }
                }
                anyhow::bail!(
                    "the `self` bundle is not acquired; run `yasm add self --action apply`"
                );
            }
            selected.extend(members);
            continue;
        }
        let id = SkillId::parse(id)?;
        if !lock.skills.contains_key(&id) {
            let exists_globally = if context.is_project() {
                let paths = YasmPaths::discover()?;
                LockFile::read(&paths.lock_file())?.skills.contains_key(&id)
            } else {
                false
            };
            if exists_globally {
                let action = if command == "info" {
                    "inspect"
                } else {
                    command
                };
                anyhow::bail!(
                        "Skill '{id}' is not installed in this project, but exists globally.\nRun `yasm {command} {id} --global` to {action} it globally."
                    );
            }
            anyhow::bail!(
                "skill `{id}` is not acquired; acquired skills: {}",
                installed_skill_ids(lock)
            );
        }
        selected.insert(id);
    }
    Ok(selected.into_iter().collect())
}

fn acquired_self_member_ids(lock: &LockFile) -> BTreeSet<SkillId> {
    lock.skills
        .iter()
        .filter(|(_, record)| {
            record.source.kind == SourceKind::Bundled && record.source.path == SELF_BUNDLE_ID
        })
        .map(|(skill_id, _)| skill_id.clone())
        .collect()
}

fn select_locked_skill_ids(
    context: &ScopeContext,
    lock: &LockFile,
    requested: &[String],
    quiet: bool,
    command: &str,
    prompt: &str,
    hint: &str,
) -> Result<Vec<SkillId>> {
    if requested.is_empty() {
        if lock.skills.is_empty() {
            if !quiet {
                println!("no skills acquired");
            }
            return Ok(Vec::new());
        }
        let skills = lock.skills.iter().collect::<Vec<_>>();
        let labels = skills
            .iter()
            .map(|(skill_id, record)| display_skill_name(skill_id, record))
            .collect::<Vec<_>>();
        let defaults = vec![false; labels.len()];
        let hint = format!("{hint}; acquired skills: {}", installed_skill_ids(lock));
        let selections = interactive::ask_multiselect("skills", prompt, &labels, &defaults, &hint)?;
        if selections.is_empty() {
            anyhow::bail!("no skills selected");
        }
        return Ok(selections
            .into_iter()
            .filter_map(|idx| skills.get(idx).map(|(skill_id, _)| (*skill_id).clone()))
            .collect());
    }

    selected_skill_ids(context, lock, requested, command)
}

fn select_add_agents<'a>(
    registry: &'a AgentRegistry,
    requested: &[LinkTarget],
    no_enable: bool,
) -> Result<Vec<&'a Agent>> {
    if no_enable {
        return Ok(Vec::new());
    }
    if !requested.is_empty() {
        return resolve_agents(registry, requested);
    }
    Ok(registry.all().iter().collect())
}

fn require_agents<'a>(
    registry: &'a AgentRegistry,
    requested: &[LinkTarget],
) -> Result<Vec<&'a Agent>> {
    if !requested.is_empty() {
        return resolve_agents(registry, requested);
    }
    let available = available_agent_ids(registry);
    anyhow::bail!("missing --agent; pass --agent <agent> (valid agents: {available})");
}

fn available_agent_ids(registry: &AgentRegistry) -> String {
    registry
        .all()
        .iter()
        .map(|agent| agent.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn resolve_agents<'a>(
    registry: &'a AgentRegistry,
    requested: &[LinkTarget],
) -> Result<Vec<&'a Agent>> {
    let mut seen = BTreeSet::new();
    let mut agents = Vec::new();
    for id in requested {
        let agent = registry.get(id.as_str())?;
        if seen.insert(agent.id.as_str().to_string()) {
            agents.push(agent);
        }
    }
    Ok(agents)
}

fn agent_ids(agents: &[&Agent]) -> BTreeSet<AgentId> {
    agents.iter().map(|agent| agent.id.clone()).collect()
}

fn display_agent_ids(agents: &[&Agent]) -> String {
    agents
        .iter()
        .map(|agent| agent.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn display_discovered_skill(skill: &DiscoveredSkill) -> String {
    skill.name.to_string()
}

fn warn_skipped_skills(progress: &progress::Progress, skipped: &[SkippedSkill]) {
    for skill in skipped {
        progress.warn(format!("skipped skill `{}`: parse error", skill.path));
    }
}

fn installed_skill_ids(lock: &LockFile) -> String {
    if lock.skills.is_empty() {
        "none".to_string()
    } else {
        lock.skills
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn utf8_temp_path(temp: &tempfile::TempDir) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(temp.path().join("source"))
        .map_err(|path| anyhow::anyhow!("non-UTF-8 temp path: {}", path.display()))
}

fn current_dir_utf8() -> Result<Utf8PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    Utf8PathBuf::from_path_buf(cwd)
        .map_err(|path| anyhow::anyhow!("non-UTF-8 current directory: {}", path.display()))
}

fn print_init_tip_if_relevant(
    context: &ScopeContext,
    explicit_global: bool,
    json_output: bool,
) -> Result<()> {
    if json_output || explicit_global || !context.is_global() || !std::io::stdout().is_terminal() {
        return Ok(());
    }

    if is_git_work_tree(&current_dir_utf8()?) {
        println!("Tip: run `yasm init` to manage skills for this repository.");
    }
    Ok(())
}

fn is_git_work_tree(path: &Utf8Path) -> bool {
    ProcessCommand::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success() && output.stdout == b"true\n")
}

fn canonical_utf8(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let canonical =
        std::fs::canonicalize(path).with_context(|| format!("failed to resolve {path}"))?;
    Utf8PathBuf::from_path_buf(canonical)
        .map_err(|path| anyhow::anyhow!("non-UTF-8 path: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn diff_formats_share_changes_without_styling_plain_output() {
        let old = DiffEntry::Text("old\n".into());
        let new = DiffEntry::Text("new\n".into());
        let (plain, terminal) = format_entry_diff(
            "installed/SKILL.md",
            "candidate/SKILL.md",
            &old,
            &new,
            Some(80),
        );
        assert!(plain.contains("-old\n+new\n"));
        assert!(!plain.contains('\x1b'));
        let terminal = terminal.expect("terminal rendering requested");
        assert!(terminal.contains("\x1b[31m- "));
        assert!(terminal.contains("\x1b[32m+ "));
        let (redirected, terminal) =
            format_entry_diff("installed/SKILL.md", "candidate/SKILL.md", &old, &new, None);
        assert_eq!(redirected, plain);
        assert!(terminal.is_none());
    }

    #[test]
    fn directory_diff_detects_empty_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let installed = root.join("installed");
        let candidate = root.join("candidate");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::create_dir_all(candidate.join("empty")).unwrap();
        std::fs::write(installed.join("SKILL.md"), "same\n").unwrap();
        std::fs::write(candidate.join("SKILL.md"), "same\n").unwrap();

        let diff =
            skill_directory_diff(&SkillId::parse("demo").unwrap(), &installed, &candidate).unwrap();

        assert!(diff.changed);
        assert!(diff.output.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn directory_diff_detects_executable_bit_changes() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let installed = root.join("installed");
        let candidate = root.join("candidate");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::create_dir_all(&candidate).unwrap();
        std::fs::write(installed.join("SKILL.md"), "same\n").unwrap();
        std::fs::write(candidate.join("SKILL.md"), "same\n").unwrap();
        std::fs::set_permissions(
            installed.join("SKILL.md"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::set_permissions(
            candidate.join("SKILL.md"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let diff =
            skill_directory_diff(&SkillId::parse("demo").unwrap(), &installed, &candidate).unwrap();

        assert!(diff.changed);
        assert!(diff.output.is_empty());
    }

    #[test]
    fn default_less_env_allows_alternate_screen() {
        let less = default_less_env("less -R", false).expect("less should get default flags");

        assert_eq!(less, "R");
    }

    #[test]
    fn default_less_env_respects_existing_or_non_less_pagers() {
        assert_eq!(default_less_env("less -R", true), None);
        assert_eq!(default_less_env("cat", false), None);
    }

    #[test]
    fn broken_pipe_from_pager_does_not_fallback_to_printing() {
        let error = std::io::Error::from(std::io::ErrorKind::BrokenPipe);

        assert!(!pager_write_error_should_fallback(&error));
    }

    #[test]
    fn source_checkout_key_ignores_skill_subpaths() {
        let root_source = SourceSpec {
            kind: SourceKind::Github,
            path: "https://github.com/owner/repo.git".to_string(),
            r#ref: Some(yasm_core::GitRef::parse("main").unwrap()),
            subpath: None,
        };
        let subpath_source = SourceSpec {
            subpath: Some(yasm_core::SkillPath::parse("plugins/example").unwrap()),
            ..root_source.clone()
        };

        assert_eq!(
            SourceCheckoutKey::from(&root_source),
            SourceCheckoutKey::from(&subpath_source)
        );
    }

    #[test]
    fn migration_catalog_contains_only_the_curated_exact_ids() {
        let entries = [
            (
                "frontend-design",
                "anthropics/skills",
                Some("skills/frontend-design"),
            ),
            (
                "find-skills",
                "vercel-labs/skills",
                Some("skills/find-skills"),
            ),
            (
                "setup-matt-pocock-skills",
                "mattpocock/skills",
                Some("skills/engineering/setup-matt-pocock-skills"),
            ),
            (
                "ask-matt",
                "mattpocock/skills",
                Some("skills/engineering/ask-matt"),
            ),
            (
                "git-guardrails-claude-code",
                "mattpocock/skills",
                Some("skills/misc/git-guardrails-claude-code"),
            ),
            (
                "migrate-to-shoehorn",
                "mattpocock/skills",
                Some("skills/misc/migrate-to-shoehorn"),
            ),
            (
                "grill-me",
                "mattpocock/skills",
                Some("skills/productivity/grill-me"),
            ),
            (
                "grill-with-docs",
                "mattpocock/skills",
                Some("skills/engineering/grill-with-docs"),
            ),
            (
                "vercel-react-best-practices",
                "vercel-labs/agent-skills",
                Some("skills/react-best-practices"),
            ),
            (
                "vercel-composition-patterns",
                "vercel-labs/agent-skills",
                Some("skills/composition-patterns"),
            ),
            (
                "vercel-react-native-skills",
                "vercel-labs/agent-skills",
                Some("skills/react-native-skills"),
            ),
            (
                "supabase-postgres-best-practices",
                "supabase/agent-skills",
                Some("skills/supabase-postgres-best-practices"),
            ),
            (
                "remotion-best-practices",
                "remotion-dev/skills",
                Some("skills/remotion-best-practices"),
            ),
            (
                "hyperframes-cli",
                "heygen-com/hyperframes",
                Some("skills/hyperframes-cli"),
            ),
            (
                "prisma-client-api",
                "prisma/skills",
                Some("prisma-client-api"),
            ),
            (
                "emil-design-eng",
                "emilkowalski/skills",
                Some("skills/emil-design-eng"),
            ),
            ("caveman", "JuliusBrussee/caveman", Some("skills/caveman")),
            (
                "caveman-commit",
                "JuliusBrussee/caveman",
                Some("skills/caveman-commit"),
            ),
            (
                "caveman-review",
                "JuliusBrussee/caveman",
                Some("skills/caveman-review"),
            ),
            (
                "caveman-stats",
                "JuliusBrussee/caveman",
                Some("skills/caveman-stats"),
            ),
            ("humanizer", "blader/humanizer", None),
            (
                "poteto-mode",
                "cursor/plugins",
                Some("pstack/skills/poteto-mode"),
            ),
            (
                "setup-pstack",
                "cursor/plugins",
                Some("pstack/skills/setup-pstack"),
            ),
        ];

        for (id, repository, subpath) in entries {
            let source = catalog_source(&SkillId::parse(id).unwrap()).unwrap();
            assert_eq!(
                source.path,
                format!("https://github.com/{repository}.git"),
                "{id} repository"
            );
            assert_eq!(
                source.subpath.as_ref().map(SkillPath::as_str),
                subpath,
                "{id} path"
            );
        }
        for generic in ["review", "tdd", "research", "architect"] {
            assert!(catalog_source(&SkillId::parse(generic).unwrap()).is_none());
        }
    }
}

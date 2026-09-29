use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context as _, Result};
use camino::Utf8PathBuf;
use serde::Serialize;

use crate::catalogs::parse_catalog;
use crate::importers::import_plugin;
use crate::lifecycle::MutationLock;
use crate::model::{
    plugin_storage_key, CatalogEntry, InstallationRecord, MarketplaceRecord, SourceFormat,
    TargetAgent,
};
use crate::sources::{fetch_marketplace, fetch_plugin_source, parse_source};
use crate::state::{load_installations, load_registry, save_registry, MarketplacePaths};
use crate::store::{digest_package, snapshot_package};
use crate::targets::{self, TargetContext};
use crate::transaction::{self, Change};
use crate::{InstalledPluginSummary, PluginComponentSummary};

pub struct Context {
    pub paths: MarketplacePaths,
    pub target: TargetContext,
}

impl Context {
    pub fn resolve(global: bool) -> Result<Self> {
        let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?).map_err(|path| {
            anyhow::anyhow!("working directory is not UTF-8: {}", path.display())
        })?;
        let project_root = if global {
            None
        } else {
            yasm_core::find_project_root(&cwd)
        };
        let yasm_paths = match &project_root {
            Some(root) => yasm_core::YasmPaths::from_project_root(root.clone())?,
            None => yasm_core::YasmPaths::discover()?,
        };
        let use_global = project_root.is_none();
        Ok(Self {
            paths: MarketplacePaths::new(&yasm_paths.data_dir, &yasm_paths.cache_dir),
            target: TargetContext::resolve(use_global, project_root.as_deref())?,
        })
    }
}

pub fn marketplace_add(
    context: &Context,
    source: &str,
    alias: Option<&str>,
    format: Option<SourceFormat>,
    json: bool,
) -> Result<()> {
    let _lock = MutationLock::acquire(&context.paths)?;
    transaction::recover(&context.paths)?;
    let source = parse_source(source)?;
    let fetched = fetch_marketplace(&source, &context.paths)?;
    let catalog = parse_catalog(&fetched.root, format)?;
    let name = alias.unwrap_or(&catalog.name).to_string();
    validate_name(&name, "marketplace name")?;
    let mut registry = load_registry(&context.paths)?;
    if registry.marketplaces.contains_key(&name) {
        bail!("marketplace `{name}` is already registered");
    }
    let entry_count = catalog.entries.len();
    registry.marketplaces.insert(
        name.clone(),
        MarketplaceRecord {
            name: name.clone(),
            source,
            format: catalog.format,
            catalog_path: catalog.path,
            resolved_revision: fetched.revision,
            entries: catalog.entries,
        },
    );
    save_registry(&context.paths, &registry)?;
    if json {
        print_json(&serde_json::json!({"name": name, "plugins": entry_count}))?;
    } else {
        println!("registered {name} ({entry_count} plugins)");
    }
    Ok(())
}

pub fn marketplace_list(context: &Context, json: bool) -> Result<()> {
    let registry = load_registry(&context.paths)?;
    if json {
        print_json(&registry.marketplaces)?;
    } else if registry.marketplaces.is_empty() {
        println!("no marketplaces registered");
    } else {
        for marketplace in registry.marketplaces.values() {
            println!(
                "{}\t{}\t{} plugins",
                marketplace.name,
                marketplace.format.as_str(),
                marketplace.entries.len()
            );
        }
    }
    Ok(())
}

pub fn marketplace_update(context: &Context, name: Option<&str>, json: bool) -> Result<()> {
    let _lock = MutationLock::acquire(&context.paths)?;
    transaction::recover(&context.paths)?;
    let mut registry = load_registry(&context.paths)?;
    let names = match name {
        Some(name) => {
            if !registry.marketplaces.contains_key(name) {
                bail!(
                    "unknown marketplace `{name}`; available: {}",
                    candidates(registry.marketplaces.keys())
                );
            }
            vec![name.to_string()]
        }
        None => registry.marketplaces.keys().cloned().collect(),
    };
    let mut updated = Vec::new();
    for name in names {
        let existing = registry
            .marketplaces
            .get(&name)
            .cloned()
            .context("marketplace disappeared while updating")?;
        let fetched = fetch_marketplace(&existing.source, &context.paths)?;
        let catalog = parse_catalog(&fetched.root, Some(existing.format))?;
        let count = catalog.entries.len();
        registry.marketplaces.insert(
            name.clone(),
            MarketplaceRecord {
                name: name.clone(),
                source: existing.source,
                format: catalog.format,
                catalog_path: catalog.path,
                resolved_revision: fetched.revision,
                entries: catalog.entries,
            },
        );
        updated.push((name, count));
    }
    save_registry(&context.paths, &registry)?;
    if json {
        print_json(&updated)?;
    } else {
        for (name, count) in updated {
            println!("updated {name} ({count} plugins)");
        }
    }
    Ok(())
}

pub fn marketplace_remove(context: &Context, name: &str) -> Result<()> {
    let _lock = MutationLock::acquire(&context.paths)?;
    transaction::recover(&context.paths)?;
    let mut registry = load_registry(&context.paths)?;
    if registry.marketplaces.remove(name).is_none() {
        bail!(
            "unknown marketplace `{name}`; available: {}",
            candidates(registry.marketplaces.keys())
        );
    }
    save_registry(&context.paths, &registry)?;
    println!("removed marketplace {name}; installed plugins were retained");
    Ok(())
}

pub fn plugin_list(
    context: &Context,
    available: bool,
    marketplace: Option<&str>,
    json: bool,
) -> Result<()> {
    if available {
        let registry = load_registry(&context.paths)?;
        let entries = available_entries(&registry.marketplaces, marketplace)?;
        if json {
            print_json(&entries)?;
        } else if entries.is_empty() {
            println!("no plugins available");
        } else {
            for entry in entries {
                println!(
                    "{}@{}\t{}",
                    entry.entry.name,
                    entry.marketplace,
                    entry.entry.description.as_deref().unwrap_or("")
                );
            }
        }
    } else {
        if marketplace.is_some() {
            bail!("--marketplace requires --available");
        }
        let installations = load_installations(&context.paths)?;
        if json {
            print_json(&installations.plugins)?;
        } else if installations.plugins.is_empty() {
            println!("no plugins installed");
        } else {
            for plugin in installations.plugins.values() {
                let enabled = plugin
                    .enabled
                    .iter()
                    .map(|target| target.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{}\t{} skill(s)\t{} MCP server(s)\t{}",
                    plugin.id,
                    plugin.definition.skills.len(),
                    plugin.definition.mcp_servers.len(),
                    enabled
                );
            }
        }
    }
    Ok(())
}

pub fn plugin_summaries(context: &Context) -> Result<Vec<InstalledPluginSummary>> {
    let installations = load_installations(&context.paths)?;
    Ok(installations
        .plugins
        .values()
        .map(|installation| {
            let mut skills = installation
                .definition
                .skills
                .iter()
                .map(|skill| PluginComponentSummary {
                    name: skill.name.clone(),
                    enabled: installation
                        .outputs
                        .iter()
                        .filter(|output| {
                            output
                                .skill_sources
                                .values()
                                .any(|path| path == &skill.path)
                        })
                        .map(|output| output.target.as_str().to_string())
                        .collect(),
                })
                .collect::<Vec<_>>();
            skills.sort_by(|left, right| left.name.cmp(&right.name));
            let mut mcp_servers = installation
                .definition
                .mcp_servers
                .iter()
                .map(|server| PluginComponentSummary {
                    name: server.name.clone(),
                    enabled: installation
                        .outputs
                        .iter()
                        .filter(|output| {
                            output
                                .mcp_sources
                                .values()
                                .any(|source| source == &server.name)
                        })
                        .map(|output| output.target.as_str().to_string())
                        .collect(),
                })
                .collect::<Vec<_>>();
            mcp_servers.sort_by(|left, right| left.name.cmp(&right.name));
            InstalledPluginSummary {
                id: installation.id.clone(),
                description: installation.definition.description.clone(),
                enabled: installation
                    .enabled
                    .iter()
                    .map(|target| target.as_str().to_string())
                    .collect(),
                skill_count: installation.definition.skills.len(),
                mcp_server_count: installation.definition.mcp_servers.len(),
                skills,
                mcp_servers,
            }
        })
        .collect())
}

pub fn plugin_info(context: &Context, selector: &str, json: bool) -> Result<()> {
    let installations = load_installations(&context.paths)?;
    let refers_to_installed = installations.plugins.contains_key(selector)
        || installations
            .plugins
            .values()
            .any(|plugin| plugin.entry_name == selector);
    if refers_to_installed {
        let id = resolve_installed(&installations.plugins, selector)?;
        let plugin = &installations.plugins[&id];
        return print_plugin_info(plugin, json);
    }
    let registry = load_registry(&context.paths)?;
    let selected = resolve_available(&registry.marketplaces, selector, None)?;
    let fetched = fetch_plugin_source(
        selected.marketplace_record,
        &selected.entry.source,
        &context.paths,
    )?;
    let definition = import_plugin(
        &fetched.root,
        selected.entry,
        selected.marketplace_record.resolved_revision.clone(),
        fetched.revision,
    )?;
    if json {
        print_json(&definition)?;
    } else {
        print_definition(
            &format!("{}@{}", selected.entry.name, selected.marketplace),
            &definition,
        );
    }
    Ok(())
}

pub fn plugin_add(
    context: &Context,
    selector: &str,
    marketplace: Option<&str>,
    target: Option<TargetAgent>,
    no_enable: bool,
    skill_aliases: &[String],
    mcp_aliases: &[String],
) -> Result<()> {
    let _lock = MutationLock::acquire(&context.paths)?;
    transaction::recover(&context.paths)?;
    let registry = load_registry(&context.paths)?;
    let selected = resolve_available(&registry.marketplaces, selector, marketplace)?;
    let id = format!("{}@{}", selected.entry.name, selected.marketplace);
    let mut installations = load_installations(&context.paths)?;
    if installations.plugins.contains_key(&id) {
        bail!("plugin `{id}` is already installed");
    }
    let fetched = fetch_plugin_source(
        selected.marketplace_record,
        &selected.entry.source,
        &context.paths,
    )?;
    let definition = import_plugin(
        &fetched.root,
        selected.entry,
        selected.marketplace_record.resolved_revision.clone(),
        fetched.revision,
    )?;
    let snapshot = snapshot_package(
        &context.paths,
        &plugin_storage_key(&id),
        &definition.import.digest,
        &fetched.root,
    )?;
    let mut installation = InstallationRecord {
        id: id.clone(),
        marketplace: selected.marketplace.to_string(),
        entry_name: selected.entry.name.clone(),
        marketplace_source: selected.marketplace_record.source.clone(),
        catalog_format: selected.marketplace_record.format,
        catalog_revision: selected.marketplace_record.resolved_revision.clone(),
        source: selected.entry.source.clone(),
        catalog_entry: selected.entry.retained_for_installation(),
        snapshot,
        definition,
        enabled: BTreeSet::new(),
        outputs: Vec::new(),
    };
    let mut changes = Vec::new();
    if !no_enable {
        let target = target
            .context("missing target agent; pass --agent <codex|claude|cursor> or --no-enable")?;
        changes.extend(enable_all_components(
            context,
            &mut installation,
            target,
            skill_aliases,
            mcp_aliases,
        )?);
    }
    transaction::commit(
        &context.paths,
        &mut installations,
        &id,
        Some(installation),
        changes,
        Vec::new(),
    )?;
    println!(
        "installed {id}{}",
        target
            .filter(|_| !no_enable)
            .map(|target| format!(" for {}", target.as_str()))
            .unwrap_or_default()
    );
    Ok(())
}

pub fn plugin_enable(
    context: &Context,
    selector: &str,
    target: TargetAgent,
    skill_aliases: &[String],
    mcp_aliases: &[String],
) -> Result<()> {
    let _lock = MutationLock::acquire(&context.paths)?;
    transaction::recover(&context.paths)?;
    let mut installations = load_installations(&context.paths)?;
    let id = resolve_installed(&installations.plugins, selector)?;
    let mut installation = installations.plugins[&id].clone();
    let changes = enable_all_components(
        context,
        &mut installation,
        target,
        skill_aliases,
        mcp_aliases,
    )?;
    transaction::commit(
        &context.paths,
        &mut installations,
        &id,
        Some(installation),
        changes,
        Vec::new(),
    )?;
    println!(
        "enabled {id} for {}; reload the target if it is already running",
        target.as_str()
    );
    Ok(())
}

pub fn plugin_disable(context: &Context, selector: &str, target: TargetAgent) -> Result<()> {
    let _lock = MutationLock::acquire(&context.paths)?;
    transaction::recover(&context.paths)?;
    let mut installations = load_installations(&context.paths)?;
    let id = resolve_installed(&installations.plugins, selector)?;
    let mut installation = installations.plugins[&id].clone();
    let changes = disable_record(context, &mut installation, target)?;
    transaction::commit(
        &context.paths,
        &mut installations,
        &id,
        Some(installation),
        changes,
        Vec::new(),
    )?;
    println!("disabled {id} for {}", target.as_str());
    Ok(())
}

pub fn plugin_update(context: &Context, selector: Option<&str>, force: bool) -> Result<()> {
    let _lock = MutationLock::acquire(&context.paths)?;
    transaction::recover(&context.paths)?;
    let mut installations = load_installations(&context.paths)?;
    let registry = load_registry(&context.paths)?;
    let ids = match selector {
        Some(selector) => vec![resolve_installed(&installations.plugins, selector)?],
        None => installations.plugins.keys().cloned().collect(),
    };
    for id in &ids {
        let old = installations.plugins[id].clone();
        let locally_modified = require_unmodified(&old, force)?;
        let registered = registry
            .marketplaces
            .get(&old.marketplace)
            .and_then(|marketplace| {
                marketplace
                    .entries
                    .iter()
                    .find(|entry| entry.name == old.entry_name)
                    .map(|entry| (marketplace.clone(), entry.clone()))
            });
        let (marketplace, entry) = match registered {
            Some(found) => found,
            None => (
                MarketplaceRecord {
                    name: old.marketplace.clone(),
                    source: old.marketplace_source.clone(),
                    format: old.catalog_format,
                    catalog_path: String::new(),
                    resolved_revision: old.catalog_revision.clone(),
                    entries: Vec::new(),
                },
                old.catalog_entry.clone(),
            ),
        };
        let fetched = fetch_plugin_source(&marketplace, &entry.source, &context.paths)?;
        let definition = import_plugin(
            &fetched.root,
            &entry,
            marketplace.resolved_revision.clone(),
            fetched.revision,
        )?;
        let mut replacement = old.clone();
        replacement.marketplace_source = marketplace.source.clone();
        replacement.catalog_format = marketplace.format;
        replacement.catalog_revision = marketplace.resolved_revision.clone();
        replacement.source = entry.source.clone();
        replacement.catalog_entry = entry.retained_for_installation();
        let snapshot =
            if definition.import.digest == old.definition.import.digest && !locally_modified {
                old.snapshot.clone()
            } else {
                snapshot_package(
                    &context.paths,
                    &replacement.storage_key(),
                    &definition.import.digest,
                    &fetched.root,
                )?
            };
        replacement.snapshot = snapshot;
        replacement.definition = definition;
        let (replacement, changes) = reconcile_update(context, &old, replacement)?;
        transaction::commit(
            &context.paths,
            &mut installations,
            id,
            Some(replacement),
            changes,
            Vec::new(),
        )?;
        println!("updated {id}");
    }
    Ok(())
}

fn reconcile_update(
    context: &Context,
    old: &InstallationRecord,
    mut replacement: InstallationRecord,
) -> Result<(InstallationRecord, Vec<Change>)> {
    replacement.outputs.clear();
    replacement.enabled.clear();
    let mut changes = Vec::new();
    for receipt in &old.outputs {
        changes.extend(targets::plan_disable(
            old,
            receipt,
            &context.target,
            &context.paths,
        )?);
        // Preserve aliases only for components still present in the replacement package.
        let skill_aliases = receipt
            .skill_sources
            .iter()
            .filter_map(|(output, path)| {
                let skill = old
                    .definition
                    .skills
                    .iter()
                    .find(|skill| &skill.path == path)?;
                (skill.name != *output
                    && replacement
                        .definition
                        .skills
                        .iter()
                        .any(|new| new.name == skill.name))
                .then(|| format!("{}={output}", skill.name))
            })
            .collect::<Vec<_>>();
        let mcp_aliases = receipt
            .mcp_sources
            .iter()
            .filter(|(output, source)| {
                output != source
                    && replacement
                        .definition
                        .mcp_servers
                        .iter()
                        .any(|server| &server.name == *source)
            })
            .map(|(output, source)| format!("{source}={output}"))
            .collect::<Vec<_>>();
        changes.extend(enable_all_components(
            context,
            &mut replacement,
            receipt.target,
            &skill_aliases,
            &mcp_aliases,
        )?);
    }
    Ok((replacement, changes))
}

pub fn plugin_remove(
    context: &Context,
    selector: &str,
    all: bool,
    purge_data: bool,
    force: bool,
) -> Result<()> {
    let _lock = MutationLock::acquire(&context.paths)?;
    transaction::recover(&context.paths)?;
    let mut installations = load_installations(&context.paths)?;
    let id = resolve_installed(&installations.plugins, selector)?;
    let mut installation = installations.plugins[&id].clone();
    require_unmodified(&installation, force)?;
    if !installation.enabled.is_empty() && !all {
        bail!(
            "plugin `{id}` is enabled for {}; pass --all to disable its managed outputs before removal",
            installation
                .enabled
                .iter()
                .map(|target| target.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut changes = Vec::new();
    let targets = installation.enabled.iter().copied().collect::<Vec<_>>();
    for target in targets {
        changes.extend(disable_record(context, &mut installation, target)?);
    }
    let mut cleanup = vec![
        context.paths.packages().join(installation.storage_key()),
        context
            .paths
            .root
            .join("outputs")
            .join(installation.storage_key()),
    ];
    if purge_data {
        cleanup.push(context.paths.data().join(installation.storage_key()));
    }
    transaction::commit(
        &context.paths,
        &mut installations,
        &id,
        None,
        changes,
        cleanup,
    )?;
    println!(
        "removed {id}; runtime data {}",
        if purge_data {
            "was deleted"
        } else {
            "was retained"
        }
    );
    Ok(())
}

fn enable_all_components(
    context: &Context,
    installation: &mut InstallationRecord,
    target: TargetAgent,
    skill_aliases: &[String],
    mcp_aliases: &[String],
) -> Result<Vec<Change>> {
    let (selected, mcp_sources) = aliased_installation(installation, skill_aliases, mcp_aliases)?;
    let (mut receipt, changes) =
        targets::plan_enable(&selected, target, &context.target, &context.paths)?;
    receipt.mcp_sources = mcp_sources;
    installation.enabled.insert(target);
    installation.outputs.push(receipt);
    Ok(changes)
}

fn disable_record(
    context: &Context,
    installation: &mut InstallationRecord,
    target: TargetAgent,
) -> Result<Vec<Change>> {
    let index = installation
        .outputs
        .iter()
        .position(|receipt| receipt.target == target)
        .with_context(|| {
            format!(
                "plugin `{}` is not enabled for {}",
                installation.id,
                target.as_str()
            )
        })?;
    let changes = targets::plan_disable(
        installation,
        &installation.outputs[index],
        &context.target,
        &context.paths,
    )?;
    installation.outputs.remove(index);
    installation.enabled.remove(&target);
    Ok(changes)
}

fn aliased_installation(
    installation: &InstallationRecord,
    skill_aliases: &[String],
    mcp_aliases: &[String],
) -> Result<(InstallationRecord, BTreeMap<String, String>)> {
    let mut selected = installation.clone();
    let skill_aliases = parse_aliases(skill_aliases, "skill alias")?;
    for source in skill_aliases.keys() {
        if !selected
            .definition
            .skills
            .iter()
            .any(|skill| &skill.name == source)
        {
            bail!("skill alias source `{source}` is not present in the enabled package");
        }
    }
    for skill in &mut selected.definition.skills {
        if let Some(alias) = skill_aliases.get(&skill.name) {
            skill.name = alias.clone();
        }
    }
    ensure_unique(
        selected.definition.skills.iter().map(|skill| &skill.name),
        "skill output",
    )?;

    let mcp_aliases = parse_aliases(mcp_aliases, "MCP alias")?;
    for source in mcp_aliases.keys() {
        if !selected
            .definition
            .mcp_servers
            .iter()
            .any(|server| &server.name == source)
        {
            bail!("MCP alias source `{source}` is not present in the enabled package");
        }
    }
    let mut mcp_sources = BTreeMap::new();
    for server in &mut selected.definition.mcp_servers {
        let source = server.name.clone();
        if let Some(alias) = mcp_aliases.get(&source) {
            server.name = alias.clone();
        }
        mcp_sources.insert(server.name.clone(), source);
    }
    ensure_unique(
        selected
            .definition
            .mcp_servers
            .iter()
            .map(|server| &server.name),
        "MCP output",
    )?;
    Ok((selected, mcp_sources))
}

fn parse_aliases(values: &[String], kind: &str) -> Result<BTreeMap<String, String>> {
    let mut aliases = BTreeMap::new();
    for value in values {
        let (source, output) = value
            .split_once('=')
            .with_context(|| format!("invalid {kind} `{value}`; expected SOURCE=OUTPUT"))?;
        validate_name(source, kind)?;
        validate_name(output, kind)?;
        if aliases
            .insert(source.to_string(), output.to_string())
            .is_some()
        {
            bail!("duplicate {kind} source `{source}`");
        }
    }
    Ok(aliases)
}

fn ensure_unique<'a>(values: impl IntoIterator<Item = &'a String>, kind: &str) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            bail!("duplicate {kind} name `{value}` after aliasing");
        }
    }
    Ok(())
}

fn print_plugin_info(plugin: &InstallationRecord, json: bool) -> Result<()> {
    if json {
        print_json(plugin)
    } else {
        println!("Name: {}", plugin.id);
        println!("Format: {}", plugin.definition.import.format.as_str());
        println!("Digest: {}", plugin.definition.import.digest);
        println!("Enabled targets:");
        if plugin.outputs.is_empty() {
            println!("  —");
        } else {
            for output in &plugin.outputs {
                println!(
                    "  {}\t{}/{} skill(s)\t{}/{} MCP server(s)",
                    output.target.as_str(),
                    output.skill_sources.len(),
                    plugin.definition.skills.len(),
                    output.mcp_names.len(),
                    plugin.definition.mcp_servers.len()
                );
            }
        }
        println!("Skills:");
        for skill in &plugin.definition.skills {
            let enabled = plugin
                .outputs
                .iter()
                .flat_map(|output| {
                    output
                        .skill_sources
                        .iter()
                        .filter(move |(_, path)| *path == &skill.path)
                        .map(move |(name, _)| {
                            if name == &skill.name {
                                output.target.as_str().to_string()
                            } else {
                                format!("{} as {name}", output.target.as_str())
                            }
                        })
                })
                .collect::<Vec<_>>();
            println!(
                "  {}\t{}\t{}",
                skill.name,
                if enabled.is_empty() {
                    "—".to_string()
                } else {
                    enabled.join(", ")
                },
                skill.description.as_deref().unwrap_or("")
            );
        }
        println!("MCP servers:");
        for server in &plugin.definition.mcp_servers {
            let enabled = plugin
                .outputs
                .iter()
                .flat_map(|output| {
                    output
                        .mcp_sources
                        .iter()
                        .filter(move |(_, source)| *source == &server.name)
                        .map(move |(name, _)| {
                            if name == &server.name {
                                output.target.as_str().to_string()
                            } else {
                                format!("{} as {name}", output.target.as_str())
                            }
                        })
                })
                .collect::<Vec<_>>();
            println!(
                "  {}\t{}\t{:?}",
                server.name,
                if enabled.is_empty() {
                    "—".to_string()
                } else {
                    enabled.join(", ")
                },
                server.transport
            );
        }
        for diagnostic in &plugin.definition.import.diagnostics {
            println!("Diagnostic: {diagnostic}");
        }
        Ok(())
    }
}

fn print_definition(id: &str, definition: &crate::model::PluginDefinition) {
    println!("Name: {id}");
    println!("Format: {}", definition.import.format.as_str());
    println!("Digest: {}", definition.import.digest);
    println!("Skills:");
    for skill in &definition.skills {
        println!(
            "  {}\t{}",
            skill.name,
            skill.description.as_deref().unwrap_or("")
        );
    }
    println!("MCP servers:");
    for server in &definition.mcp_servers {
        println!("  {}\t{:?}", server.name, server.transport);
    }
    for diagnostic in &definition.import.diagnostics {
        println!("Diagnostic: {diagnostic}");
    }
}

fn resolve_installed(
    plugins: &BTreeMap<String, InstallationRecord>,
    selector: &str,
) -> Result<String> {
    if plugins.contains_key(selector) {
        return Ok(selector.to_string());
    }
    let matches = plugins
        .values()
        .filter(|plugin| plugin.entry_name == selector)
        .map(|plugin| plugin.id.clone())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [id] => Ok(id.clone()),
        [] => bail!(
            "unknown installed plugin `{selector}`; available: {}",
            candidates(plugins.keys())
        ),
        _ => bail!(
            "plugin name `{selector}` is ambiguous; use one of: {}",
            matches.join(", ")
        ),
    }
}

struct Available<'a> {
    marketplace: &'a str,
    marketplace_record: &'a MarketplaceRecord,
    entry: &'a CatalogEntry,
}

#[derive(Serialize)]
struct AvailableOutput<'a> {
    marketplace: &'a str,
    entry: &'a CatalogEntry,
    skill_count: Option<usize>,
    mcp_server_count: Option<usize>,
}

fn available_entries<'a>(
    marketplaces: &'a BTreeMap<String, MarketplaceRecord>,
    filter: Option<&str>,
) -> Result<Vec<AvailableOutput<'a>>> {
    if let Some(filter) = filter {
        if !marketplaces.contains_key(filter) {
            bail!(
                "unknown marketplace `{filter}`; available: {}",
                candidates(marketplaces.keys())
            );
        }
    }
    Ok(marketplaces
        .iter()
        .filter(|(name, _)| filter.is_none_or(|filter| name.as_str() == filter))
        .flat_map(|(name, marketplace)| {
            marketplace
                .entries
                .iter()
                .map(move |entry| AvailableOutput {
                    marketplace: name,
                    entry,
                    skill_count: None,
                    mcp_server_count: None,
                })
        })
        .collect())
}

fn resolve_available<'a>(
    marketplaces: &'a BTreeMap<String, MarketplaceRecord>,
    selector: &str,
    filter: Option<&str>,
) -> Result<Available<'a>> {
    let (name, qualified) = selector
        .rsplit_once('@')
        .map_or((selector, None), |(name, marketplace)| {
            (name, Some(marketplace))
        });
    let wanted_marketplace = filter.or(qualified);
    if let (Some(filter), Some(qualified)) = (filter, qualified) {
        if filter != qualified {
            bail!("plugin qualifier `{qualified}` conflicts with --marketplace {filter}");
        }
    }
    let matches = marketplaces
        .iter()
        .filter(|(marketplace, _)| {
            wanted_marketplace.is_none_or(|wanted| marketplace.as_str() == wanted)
        })
        .flat_map(|(marketplace, record)| {
            record
                .entries
                .iter()
                .filter(move |entry| entry.name == name)
                .map(move |entry| Available {
                    marketplace,
                    marketplace_record: record,
                    entry,
                })
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [selected] => Ok(Available {
            marketplace: selected.marketplace,
            marketplace_record: selected.marketplace_record,
            entry: selected.entry,
        }),
        [] => {
            let all = marketplaces
                .iter()
                .flat_map(|(marketplace, record)| {
                    record
                        .entries
                        .iter()
                        .map(move |entry| format!("{}@{marketplace}", entry.name))
                })
                .collect::<Vec<_>>();
            bail!(
                "unknown available plugin `{selector}`; available: {}",
                all.join(", ")
            )
        }
        _ => bail!(
            "plugin name `{name}` is ambiguous; use one of: {}",
            matches
                .iter()
                .map(|item| format!("{}@{}", item.entry.name, item.marketplace))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn validate_name(value: &str, kind: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value
            .chars()
            .any(|character| matches!(character, '/' | '\\' | '@'))
    {
        bail!("invalid {kind} `{value}`");
    }
    Ok(())
}

fn require_unmodified(installation: &InstallationRecord, force: bool) -> Result<bool> {
    let actual = digest_package(&installation.snapshot)?;
    let modified = actual != installation.definition.import.digest;
    if modified && !force {
        bail!(
            "plugin `{}` has local package edits; preserve them separately or pass --force to replace/remove the snapshot",
            installation.id
        );
    }
    Ok(modified)
}

fn candidates<'a, T>(values: impl IntoIterator<Item = &'a T>) -> String
where
    T: AsRef<str> + 'a + ?Sized,
{
    let values = values.into_iter().map(AsRef::as_ref).collect::<Vec<_>>();
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

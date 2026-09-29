use anyhow::Result;
use clap::{Args, Subcommand};

use crate::model::{SourceFormat, TargetAgent};
use crate::service::{self, Context};

#[derive(Debug, Args)]
pub struct MarketplaceArgs {
    #[command(subcommand)]
    command: MarketplaceCommand,
}

#[derive(Debug, Subcommand)]
enum MarketplaceCommand {
    #[command(about = "Register and fetch a plugin marketplace catalog")]
    Add {
        #[arg(help = "Local path, GitHub owner/repo shorthand, or repository URL")]
        source: String,
        #[arg(long, help = "Registry name to use instead of the catalog name")]
        alias: Option<String>,
        #[arg(
            long,
            value_enum,
            help = "Catalog format when a repository contains multiple formats"
        )]
        format: Option<SourceFormat>,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
        #[arg(long, help = "Print the result as JSON")]
        json: bool,
    },
    #[command(about = "List registered plugin marketplaces")]
    List {
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
        #[arg(long, help = "Print results as JSON")]
        json: bool,
    },
    #[command(about = "Update one or all registered marketplace catalogs")]
    Update {
        #[arg(help = "Marketplace name; omit to update all")]
        name: Option<String>,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
        #[arg(long, help = "Print results as JSON")]
        json: bool,
    },
    #[command(about = "Unregister a marketplace without removing installed plugins")]
    Remove {
        name: String,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
    },
}

#[derive(Debug, Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    command: PluginCommand,
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    #[command(about = "List installed or available plugins")]
    List {
        #[arg(long, help = "List catalog entries instead of installed plugins")]
        available: bool,
        #[arg(
            long,
            requires = "available",
            help = "Limit available entries to this marketplace"
        )]
        marketplace: Option<String>,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
        #[arg(long, help = "Print results as JSON")]
        json: bool,
    },
    #[command(about = "Inspect an installed or available plugin")]
    Info {
        plugin: String,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
        #[arg(long, help = "Print resolved inventory as JSON")]
        json: bool,
    },
    #[command(about = "Acquire a plugin and optionally enable supported components")]
    Add {
        plugin: String,
        #[arg(long, help = "Select the marketplace for an unqualified plugin name")]
        marketplace: Option<String>,
        #[arg(
            long,
            value_enum,
            required_unless_present = "no_enable",
            help = "Target agent"
        )]
        agent: Option<TargetAgent>,
        #[arg(
            long,
            conflicts_with = "agent",
            help = "Acquire without enabling components"
        )]
        no_enable: bool,
        #[arg(
            long,
            conflicts_with = "no_enable",
            value_name = "SOURCE=OUTPUT",
            help = "Rename a skill output; may be repeated"
        )]
        skill_alias: Vec<String>,
        #[arg(
            long,
            conflicts_with = "no_enable",
            value_name = "SOURCE=OUTPUT",
            help = "Rename an MCP output; may be repeated"
        )]
        mcp_alias: Vec<String>,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
    },
    #[command(about = "Enable an installed plugin for an agent")]
    Enable {
        plugin: String,
        #[arg(long, value_enum, required = true, help = "Target agent")]
        agent: TargetAgent,
        #[arg(
            long,
            value_name = "SOURCE=OUTPUT",
            help = "Rename a skill output; may be repeated"
        )]
        skill_alias: Vec<String>,
        #[arg(
            long,
            value_name = "SOURCE=OUTPUT",
            help = "Rename an MCP output; may be repeated"
        )]
        mcp_alias: Vec<String>,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
    },
    #[command(about = "Disable a plugin's Yasm-owned outputs for an agent")]
    Disable {
        plugin: String,
        #[arg(long, value_enum, required = true, help = "Target agent")]
        agent: TargetAgent,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
    },
    #[command(about = "Update one or all installed plugins and reconcile enabled outputs")]
    Update {
        plugin: Option<String>,
        #[arg(long, help = "Replace locally edited package snapshots")]
        force: bool,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
    },
    #[command(about = "Remove an installed plugin and its managed outputs")]
    Remove {
        plugin: String,
        #[arg(long, help = "Disable all active outputs before removal")]
        all: bool,
        #[arg(long, help = "Also delete persistent plugin runtime data")]
        purge_data: bool,
        #[arg(long, help = "Remove a locally edited package snapshot")]
        force: bool,
        #[arg(long, help = "Use the global marketplace store")]
        global: bool,
    },
}

pub fn run_marketplace(args: MarketplaceArgs) -> Result<()> {
    match args.command {
        MarketplaceCommand::Add {
            source,
            alias,
            format,
            global,
            json,
        } => service::marketplace_add(
            &Context::resolve(global)?,
            &source,
            alias.as_deref(),
            format,
            json,
        ),
        MarketplaceCommand::List { global, json } => {
            service::marketplace_list(&Context::resolve(global)?, json)
        }
        MarketplaceCommand::Update { name, global, json } => {
            service::marketplace_update(&Context::resolve(global)?, name.as_deref(), json)
        }
        MarketplaceCommand::Remove { name, global } => {
            service::marketplace_remove(&Context::resolve(global)?, &name)
        }
    }
}

pub fn run_plugin(args: PluginArgs) -> Result<()> {
    match args.command {
        PluginCommand::List {
            available,
            marketplace,
            global,
            json,
        } => service::plugin_list(
            &Context::resolve(global)?,
            available,
            marketplace.as_deref(),
            json,
        ),
        PluginCommand::Info {
            plugin,
            global,
            json,
        } => service::plugin_info(&Context::resolve(global)?, &plugin, json),
        PluginCommand::Add {
            plugin,
            marketplace,
            agent,
            no_enable,
            skill_alias,
            mcp_alias,
            global,
        } => service::plugin_add(
            &Context::resolve(global)?,
            &plugin,
            marketplace.as_deref(),
            agent,
            no_enable,
            &skill_alias,
            &mcp_alias,
        ),
        PluginCommand::Enable {
            plugin,
            agent,
            skill_alias,
            mcp_alias,
            global,
        } => service::plugin_enable(
            &Context::resolve(global)?,
            &plugin,
            agent,
            &skill_alias,
            &mcp_alias,
        ),
        PluginCommand::Disable {
            plugin,
            agent,
            global,
        } => service::plugin_disable(&Context::resolve(global)?, &plugin, agent),
        PluginCommand::Update {
            plugin,
            force,
            global,
        } => service::plugin_update(&Context::resolve(global)?, plugin.as_deref(), force),
        PluginCommand::Remove {
            plugin,
            all,
            purge_data,
            force,
            global,
        } => service::plugin_remove(&Context::resolve(global)?, &plugin, all, purge_data, force),
    }
}

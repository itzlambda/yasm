//! Experimental marketplace and plugin support for Yasm.
//!
//! APIs, behavior, and stored data formats may change incompatibly, and this
//! feature may be removed in a future release.

use serde::Serialize;

pub mod cli;

mod catalogs;
mod exports;
mod importers;
mod lifecycle;
mod model;
mod secrets;
mod service;
mod sources;
mod state;
mod store;
mod targets;
mod transaction;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginComponentSummary {
    pub name: String,
    pub enabled: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledPluginSummary {
    pub id: String,
    pub description: Option<String>,
    pub enabled: Vec<String>,
    pub skill_count: usize,
    pub mcp_server_count: usize,
    pub skills: Vec<PluginComponentSummary>,
    pub mcp_servers: Vec<PluginComponentSummary>,
}

pub fn installed_plugin_summaries(global: bool) -> anyhow::Result<Vec<InstalledPluginSummary>> {
    service::plugin_summaries(&service::Context::resolve(global)?)
}

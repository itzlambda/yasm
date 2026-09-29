use std::collections::BTreeMap;

use crate::transaction::{Change, OutputKind};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use toml_edit::{value, Array, DocumentMut, Item, Table};

use crate::model::{
    InstallationRecord, McpServerDefinition, McpTransport, OutputReceipt, TargetAgent,
};
use crate::secrets::{literal_credential_error, literal_credential_location};
use crate::state::MarketplacePaths;

pub struct TargetContext {
    pub global: bool,
    pub root: Utf8PathBuf,
    pub skill_root: Utf8PathBuf,
}

impl TargetContext {
    pub fn resolve(global: bool, project_root: Option<&Utf8Path>) -> Result<Self> {
        if !global {
            if let Some(root) = project_root {
                return Ok(Self {
                    global: false,
                    root: root.to_path_buf(),
                    skill_root: root.to_path_buf(),
                });
            }
        }
        let root = Utf8PathBuf::from_path_buf(etcetera::home_dir()?)
            .map_err(|path| anyhow::anyhow!("home path is not UTF-8: {}", path.display()))?;
        let skill_root = if let Ok(root) = std::env::var("YASM_AGENT_SKILLS_ROOT") {
            if root.is_empty() {
                bail!("YASM_AGENT_SKILLS_ROOT is set but empty");
            }
            Utf8PathBuf::from(root)
        } else {
            root.clone()
        };
        Ok(Self {
            global: true,
            root,
            skill_root,
        })
    }

    fn skill_dir(&self, target: TargetAgent) -> Utf8PathBuf {
        self.skill_root.join(match target {
            TargetAgent::Codex => ".agents/skills",
            TargetAgent::Claude => ".claude/skills",
            TargetAgent::Cursor => ".cursor/skills",
        })
    }

    fn mcp_config(&self, target: TargetAgent) -> Utf8PathBuf {
        match (self.global, target) {
            (false, TargetAgent::Codex) => self.root.join(".codex/config.toml"),
            (false, TargetAgent::Claude) => self.root.join(".mcp.json"),
            (false, TargetAgent::Cursor) => self.root.join(".cursor/mcp.json"),
            (true, TargetAgent::Codex) => std::env::var("CODEX_HOME")
                .map(Utf8PathBuf::from)
                .unwrap_or_else(|_| self.root.join(".codex"))
                .join("config.toml"),
            (true, TargetAgent::Claude) => self.root.join(".claude.json"),
            (true, TargetAgent::Cursor) => self.root.join(".cursor/mcp.json"),
        }
    }
}

pub fn plan_enable(
    installation: &InstallationRecord,
    target: TargetAgent,
    context: &TargetContext,
    paths: &MarketplacePaths,
) -> Result<(OutputReceipt, Vec<Change>)> {
    if installation.enabled.contains(&target) {
        bail!(
            "plugin `{}` is already enabled for {}",
            installation.id,
            target.as_str()
        );
    }
    if !installation.definition.import.excluded.is_empty()
        || !installation.definition.import.diagnostics.is_empty()
        || installation
            .definition
            .skills
            .iter()
            .any(|skill| !skill.diagnostics.is_empty())
        || installation.definition.mcp_servers.iter().any(|server| {
            !matches!(server.transport, McpTransport::Stdio | McpTransport::Http)
                || !server.extensions.is_empty()
        })
    {
        bail!("plugin `{}` contains unsupported behavior; enablement is all-or-nothing. Inspect it with `yasm plugin info {} --json`", installation.id, installation.id);
    }
    if installation.definition.skills.is_empty() && installation.definition.mcp_servers.is_empty() {
        bail!("plugin `{}` has no compatible components", installation.id);
    }
    let mut receipt = OutputReceipt {
        target,
        skill_links: Vec::new(),
        skill_sources: BTreeMap::new(),
        skill_exports: BTreeMap::new(),
        mcp_config: None,
        mcp_names: Vec::new(),
        mcp_sources: BTreeMap::new(),
    };
    let mut changes = Vec::new();
    let data = paths.data().join(installation.storage_key());
    for server in &installation.definition.mcp_servers {
        if let Some(location) = literal_credential_location(server) {
            bail!("{}", literal_credential_error(&server.name, &location));
        }
        let config = context.mcp_config(target);
        changes.push(Change {
            kind: config_kind(target),
            path: config.clone(),
            name: server.name.clone(),
            before: None,
            after: Some(render_server(target, server, installation, context, &data)?),
        });
        receipt.mcp_config = Some(config);
        receipt.mcp_names.push(server.name.clone());
    }
    for skill in &installation.definition.skills {
        let source = crate::exports::prepare_skill(installation, &skill.path, paths)?;
        let link = context.skill_dir(target).join(&skill.name);
        let target_path = skill_link_target(context, &link, &source)?;
        changes.push(Change {
            kind: OutputKind::SkillLink,
            path: link.clone(),
            name: skill.name.clone(),
            before: None,
            after: Some(Value::String(target_path.to_string())),
        });
        receipt.skill_links.push(link);
        receipt
            .skill_sources
            .insert(skill.name.clone(), skill.path.clone());
        receipt.skill_exports.insert(skill.name.clone(), source);
    }
    Ok((receipt, changes))
}

fn config_kind(target: TargetAgent) -> OutputKind {
    match target {
        TargetAgent::Codex => OutputKind::TomlMcp,
        _ => OutputKind::JsonMcp,
    }
}
fn render_server(
    target: TargetAgent,
    server: &McpServerDefinition,
    installation: &InstallationRecord,
    context: &TargetContext,
    data: &Utf8Path,
) -> Result<Value> {
    if target == TargetAgent::Codex {
        let mut doc = DocumentMut::new();
        doc["server"] = Item::Table(render_codex_server(server, installation, context, data)?);
        let parsed: Value = toml_edit::de::from_str(&doc.to_string())?;
        Ok(parsed["server"].clone())
    } else {
        render_json_server(target, server, installation, context, data)
    }
}

pub fn plan_disable(
    installation: &InstallationRecord,
    receipt: &OutputReceipt,
    context: &TargetContext,
    paths: &MarketplacePaths,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for link in &receipt.skill_links {
        let name = link.file_name().context("skill output has no name")?;
        let source = receipt
            .skill_exports
            .get(name)
            .context("skill receipt has no source")?;
        if source.exists() {
            let expected = source.file_name().context("skill export has no digest")?;
            if crate::store::digest_skill_export(source)? != expected {
                bail!("refusing to remove modified skill export {source}");
            }
        }
        let target_path = skill_link_target(context, link, source)?;
        let change = Change {
            kind: OutputKind::SkillLink,
            path: link.clone(),
            name: name.to_string(),
            before: Some(Value::String(target_path.to_string())),
            after: None,
        };
        if change.current()?.is_some() {
            changes.push(change);
        }
    }
    if let Some(config) = &receipt.mcp_config {
        let data = paths.data().join(installation.storage_key());
        for server in receipt_servers(installation, receipt)? {
            let change = Change {
                kind: config_kind(receipt.target),
                path: config.clone(),
                name: server.name.clone(),
                before: Some(render_server(
                    receipt.target,
                    &server,
                    installation,
                    context,
                    &data,
                )?),
                after: None,
            };
            if change.current()?.is_some() {
                changes.push(change);
            }
        }
    }
    Ok(changes)
}

fn skill_link_target(
    context: &TargetContext,
    link: &Utf8Path,
    source: &Utf8Path,
) -> Result<Utf8PathBuf> {
    if context.global {
        return Ok(source.to_path_buf());
    }
    let from = link.parent().context("skill output path has no parent")?;
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = source.components().collect::<Vec<_>>();
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        bail!("cannot construct a relative project skill link from {link} to {source}");
    }
    let mut relative = Utf8PathBuf::new();
    for _ in common..from_components.len() {
        relative.push("..");
    }
    for component in &to_components[common..] {
        relative.push(component.as_str());
    }
    Ok(relative)
}

fn receipt_servers(
    installation: &InstallationRecord,
    receipt: &OutputReceipt,
) -> Result<Vec<McpServerDefinition>> {
    receipt
        .mcp_names
        .iter()
        .map(|output_name| {
            let source_name = receipt
                .mcp_sources
                .get(output_name)
                .context("MCP receipt has no source")?;
            let mut server = installation
                .definition
                .mcp_servers
                .iter()
                .find(|server| &server.name == source_name)
                .cloned()
                .context("MCP receipt refers to a missing server")?;
            server.name = output_name.clone();
            Ok(server)
        })
        .collect()
}

fn render_json_server(
    target: TargetAgent,
    server: &McpServerDefinition,
    installation: &InstallationRecord,
    context: &TargetContext,
    data_dir: &Utf8Path,
) -> Result<Value> {
    let mut value = Map::new();
    match server.transport {
        McpTransport::Stdio => {
            value.insert("type".to_string(), Value::String("stdio".to_string()));
            value.insert(
                "command".to_string(),
                Value::String(render_text(
                    server
                        .command
                        .as_deref()
                        .context("stdio server has no command")?,
                    target,
                    installation,
                    context,
                    data_dir,
                )?),
            );
            if !server.args.is_empty() {
                value.insert(
                    "args".to_string(),
                    Value::Array(
                        server
                            .args
                            .iter()
                            .map(|arg| {
                                render_text(arg, target, installation, context, data_dir)
                                    .map(Value::String)
                            })
                            .collect::<Result<Vec<_>>>()?,
                    ),
                );
            }
            if server.cwd.is_some() && target != TargetAgent::Codex {
                bail!(
                    "MCP server `{}` requires a working directory, which {} standalone configuration cannot represent",
                    server.name,
                    target.as_str()
                );
            }
        }
        McpTransport::Http => {
            value.insert("type".to_string(), Value::String("http".to_string()));
            value.insert(
                "url".to_string(),
                Value::String(render_text(
                    server.url.as_deref().context("HTTP server has no URL")?,
                    target,
                    installation,
                    context,
                    data_dir,
                )?),
            );
        }
        _ => bail!("unsupported MCP transport for `{}`", server.name),
    }
    if !server.env.is_empty() {
        value.insert(
            "env".to_string(),
            Value::Object(render_map(
                &server.env,
                target,
                installation,
                context,
                data_dir,
            )?),
        );
    }
    if !server.headers.is_empty() {
        value.insert(
            "headers".to_string(),
            Value::Object(render_map(
                &server.headers,
                target,
                installation,
                context,
                data_dir,
            )?),
        );
    }
    Ok(Value::Object(value))
}

fn render_codex_server(
    server: &McpServerDefinition,
    installation: &InstallationRecord,
    context: &TargetContext,
    data_dir: &Utf8Path,
) -> Result<Table> {
    let mut table = Table::new();
    match server.transport {
        McpTransport::Stdio => {
            table.insert(
                "command",
                value(render_text(
                    server
                        .command
                        .as_deref()
                        .context("stdio server has no command")?,
                    TargetAgent::Codex,
                    installation,
                    context,
                    data_dir,
                )?),
            );
            if !server.args.is_empty() {
                let mut array = Array::new();
                for arg in &server.args {
                    array.push(render_text(
                        arg,
                        TargetAgent::Codex,
                        installation,
                        context,
                        data_dir,
                    )?);
                }
                table.insert("args", value(array));
            }
            if let Some(cwd) = &server.cwd {
                table.insert(
                    "cwd",
                    value(render_text(
                        cwd,
                        TargetAgent::Codex,
                        installation,
                        context,
                        data_dir,
                    )?),
                );
            }
        }
        McpTransport::Http => {
            table.insert(
                "url",
                value(render_text(
                    server.url.as_deref().context("HTTP server has no URL")?,
                    TargetAgent::Codex,
                    installation,
                    context,
                    data_dir,
                )?),
            );
        }
        _ => bail!("unsupported MCP transport for `{}`", server.name),
    }
    let mut env = Table::new();
    let mut env_vars = Array::new();
    for (key, raw) in &server.env {
        if env_reference(raw).is_some_and(|name| name == key) {
            env_vars.push(key.as_str());
        } else {
            env.insert(
                key,
                value(render_text(
                    raw,
                    TargetAgent::Codex,
                    installation,
                    context,
                    data_dir,
                )?),
            );
        }
    }
    if !env.is_empty() {
        table.insert("env", Item::Table(env));
    }
    if !env_vars.is_empty() {
        table.insert("env_vars", value(env_vars));
    }
    let mut literal_headers = Table::new();
    let mut env_headers = Table::new();
    for (key, raw) in &server.headers {
        if let Some(name) = env_reference(raw) {
            env_headers.insert(key, value(name));
        } else if let Some(name) = bearer_env_reference(raw) {
            if key.eq_ignore_ascii_case("authorization") {
                table.insert("bearer_token_env_var", value(name));
            } else {
                bail!("header `{key}` uses a bearer environment reference that Codex cannot map");
            }
        } else {
            literal_headers.insert(
                key,
                value(render_text(
                    raw,
                    TargetAgent::Codex,
                    installation,
                    context,
                    data_dir,
                )?),
            );
        }
    }
    if !literal_headers.is_empty() {
        table.insert("http_headers", Item::Table(literal_headers));
    }
    if !env_headers.is_empty() {
        table.insert("env_http_headers", Item::Table(env_headers));
    }
    Ok(table)
}

fn render_map(
    values: &BTreeMap<String, String>,
    target: TargetAgent,
    installation: &InstallationRecord,
    context: &TargetContext,
    data_dir: &Utf8Path,
) -> Result<Map<String, Value>> {
    values
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                Value::String(render_text(value, target, installation, context, data_dir)?),
            ))
        })
        .collect()
}

fn render_text(
    raw: &str,
    target: TargetAgent,
    installation: &InstallationRecord,
    context: &TargetContext,
    data_dir: &Utf8Path,
) -> Result<String> {
    let mut rendered = raw.to_string();
    for marker in [
        "${PLUGIN_ROOT}",
        "${CLAUDE_PLUGIN_ROOT}",
        "${CURSOR_PLUGIN_ROOT}",
    ] {
        rendered = rendered.replace(marker, installation.snapshot.as_str());
    }
    for marker in [
        "${PLUGIN_DATA}",
        "${CLAUDE_PLUGIN_DATA}",
        "${CURSOR_PLUGIN_DATA}",
    ] {
        rendered = rendered.replace(marker, data_dir.as_str());
    }
    for marker in [
        "${PROJECT_ROOT}",
        "${CLAUDE_PROJECT_DIR}",
        "${CURSOR_PROJECT_ROOT}",
    ] {
        rendered = rendered.replace(marker, context.root.as_str());
    }
    if rendered.contains("${input:") {
        bail!("unresolved plugin input reference in `{raw}`");
    }
    if target == TargetAgent::Cursor {
        rendered = cursor_env_syntax(&rendered)?;
    } else if target == TargetAgent::Claude {
        rendered = claude_env_syntax(&rendered)?;
    } else if target == TargetAgent::Codex && contains_env_expression(&rendered) {
        bail!(
            "Codex cannot safely render embedded environment expression `{raw}`; use a whole-value environment reference"
        );
    }
    Ok(rendered)
}

fn claude_env_syntax(value: &str) -> Result<String> {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${env:") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 6..];
        let end = after
            .find('}')
            .context("unterminated environment reference")?;
        let name = &after[..end];
        if !valid_env_name(name) {
            bail!("invalid Cursor environment reference `${{env:{name}}}`");
        }
        output.push_str("${");
        output.push_str(name);
        output.push('}');
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn cursor_env_syntax(value: &str) -> Result<String> {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .context("unterminated environment reference")?;
        let expression = &after[..end];
        if expression.starts_with("env:") {
            output.push_str("${");
            output.push_str(expression);
            output.push('}');
        } else if expression.contains(":-") {
            bail!(
                "Cursor cannot preserve environment fallback `${{{expression}}}`; remove the fallback or configure a literal value"
            );
        } else if valid_env_name(expression) {
            output.push_str("${env:");
            output.push_str(expression);
            output.push('}');
        } else {
            output.push_str("${");
            output.push_str(expression);
            output.push('}');
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn env_reference(value: &str) -> Option<&str> {
    value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .map(|name| name.strip_prefix("env:").unwrap_or(name))
        .filter(|name| valid_env_name(name))
}

fn bearer_env_reference(value: &str) -> Option<&str> {
    value.strip_prefix("Bearer ").and_then(env_reference)
}

fn contains_env_expression(value: &str) -> bool {
    value.contains("${")
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_translates_environment_references_and_rejects_defaults() {
        assert_eq!(
            cursor_env_syntax("Bearer ${TOKEN}").unwrap(),
            "Bearer ${env:TOKEN}"
        );
        assert!(cursor_env_syntax("${TOKEN:-fallback}").is_err());
    }

    #[test]
    fn claude_translates_cursor_environment_references() {
        assert_eq!(
            claude_env_syntax("Bearer ${env:TOKEN}").unwrap(),
            "Bearer ${TOKEN}"
        );
        assert!(claude_env_syntax("${env:not-valid!}").is_err());
    }

    #[test]
    fn codex_recognizes_whole_value_and_bearer_references() {
        assert_eq!(env_reference("${TOKEN}"), Some("TOKEN"));
        assert_eq!(env_reference("${env:TOKEN}"), Some("TOKEN"));
        assert_eq!(bearer_env_reference("Bearer ${TOKEN}"), Some("TOKEN"));
        assert_eq!(env_reference("${TOKEN:-x}"), None);
    }
}

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use yasm_core::read_skill_metadata;

use crate::model::{
    is_sensitive_env_name, CatalogEntry, ExcludedComponent, ImportRecord, InputDefinition,
    McpServerDefinition, McpTransport, PluginDefinition, SkillDefinition, SourceFormat,
};
use crate::secrets::{
    keyed_value_has_literal_credential, literal_credential_error, literal_credential_location,
    value_has_literal_credential,
};
use crate::store::digest_package;

pub fn import_plugin(
    root: &Utf8Path,
    entry: &CatalogEntry,
    catalog_revision: Option<String>,
    package_revision: Option<String>,
) -> Result<PluginDefinition> {
    let manifest_path = manifest_path(root, entry.format)?;
    let manifest = match &manifest_path {
        Some(path) => read_json(path)?,
        None => Value::Object(Map::new()),
    };
    let manifest_object = manifest
        .as_object()
        .context("plugin manifest must be a JSON object")?;
    let name = manifest_object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&entry.name)
        .to_string();
    let description = manifest_object
        .get("description")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| entry.description.clone());
    let version = manifest_object
        .get("version")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let component_manifest = if entry.format == SourceFormat::Codex
        && manifest_path
            .as_ref()
            .is_some_and(|path| path.ends_with(".codex-plugin/plugin.json"))
    {
        manifest_object
            .get("extensions")
            .and_then(Value::as_object)
            .and_then(|extensions| extensions.get("com.openai"))
            .and_then(Value::as_object)
            .unwrap_or(manifest_object)
    } else {
        manifest_object
    };
    let mut diagnostics = Vec::new();
    let claude_catalog_skills = if entry.format == SourceFormat::Claude
        && matches!(
            &entry.source,
            crate::model::PluginSource::Relative { path }
                if matches!(path.as_str(), "." | "./")
        ) {
        entry.raw.get("skills")
    } else {
        None
    };
    let skill_paths = skill_paths(
        root,
        entry.format,
        component_manifest,
        claude_catalog_skills,
        &mut diagnostics,
    )?;
    let skills = import_skills(root, &skill_paths, &mut diagnostics)?;
    let (mcp_servers, mcp_conflicts) =
        import_mcp(root, entry.format, component_manifest, &mut diagnostics)?;
    let inputs = import_inputs(component_manifest, &mut diagnostics)?;
    let mut excluded = excluded_components(root, component_manifest)?;
    excluded.extend(mcp_conflicts.into_iter().map(|name| {
        ExcludedComponent {
            kind: "mcp_conflict".to_string(),
            location: name,
            reason:
                "conflicting MCP definitions were preserved in the source package but not activated"
                    .to_string(),
        }
    }));
    if !excluded.is_empty() {
        diagnostics.push(format!(
            "{} deferred component location(s) were preserved but not activated",
            excluded.len()
        ));
    }

    Ok(PluginDefinition {
        name,
        description,
        version,
        skills,
        mcp_servers,
        inputs,
        import: ImportRecord {
            format: entry.format,
            manifests: manifest_path
                .and_then(|path| path.strip_prefix(root).ok().map(|path| path.to_string()))
                .into_iter()
                .collect(),
            marketplace_entry: entry.name.clone(),
            catalog_revision,
            package_revision,
            digest: digest_package(root)?,
            diagnostics,
            excluded,
        },
    })
}

fn manifest_path(root: &Utf8Path, format: SourceFormat) -> Result<Option<Utf8PathBuf>> {
    let candidates: &[&str] = match format {
        SourceFormat::Codex => &["plugin.json", ".codex-plugin/plugin.json"],
        SourceFormat::Claude => &[".claude-plugin/plugin.json"],
        SourceFormat::Cursor => &["plugin.json", ".cursor-plugin/plugin.json"],
    };
    for candidate in candidates {
        if root.join(candidate).is_file() {
            return Ok(Some(checked_component_path(root, candidate)?));
        }
    }
    Ok(None)
}

fn read_json(path: &Utf8Path) -> Result<Value> {
    serde_json::from_slice(&std::fs::read(path).with_context(|| format!("failed to read {path}"))?)
        .with_context(|| format!("failed to parse {path}"))
}

fn skill_paths(
    root: &Utf8Path,
    format: SourceFormat,
    manifest: &Map<String, Value>,
    catalog_override: Option<&Value>,
    diagnostics: &mut Vec<String>,
) -> Result<Vec<Utf8PathBuf>> {
    let declared = if let Some(override_value) = catalog_override {
        Some(string_list(override_value)?)
    } else if format == SourceFormat::Codex && root.join("plugin.json").is_file() {
        None
    } else {
        manifest.get("skills").map(string_list).transpose()?
    };
    let mut roots = Vec::new();
    match (format, declared) {
        (SourceFormat::Cursor, Some(paths)) | (SourceFormat::Codex, Some(paths)) => {
            roots.extend(paths);
        }
        (SourceFormat::Claude, Some(paths)) if catalog_override.is_some() => {
            roots.extend(paths);
        }
        (SourceFormat::Claude, Some(paths)) => {
            if root.join("skills").is_dir() {
                roots.push("skills".to_string());
            }
            roots.extend(paths);
        }
        (_, None) if root.join("skills").is_dir() => roots.push("skills".to_string()),
        (_, None) if root.join("SKILL.md").is_file() => roots.push(".".to_string()),
        _ => {}
    }

    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for relative in roots {
        let path = checked_component_path(root, &relative)?;
        if path.join("SKILL.md").is_file() {
            if seen.insert(path.clone()) {
                result.push(path);
            }
            continue;
        }
        if !path.is_dir() {
            diagnostics.push(format!("declared skill path does not exist: {relative}"));
            continue;
        }
        for child in sorted_dirs(&path)? {
            if child.join("SKILL.md").is_file() && seen.insert(child.clone()) {
                result.push(child);
            }
        }
    }
    Ok(result)
}

fn import_skills(
    root: &Utf8Path,
    paths: &[Utf8PathBuf],
    diagnostics: &mut Vec<String>,
) -> Result<Vec<SkillDefinition>> {
    let mut result = Vec::new();
    let mut names = BTreeSet::new();
    for path in paths {
        let metadata = read_skill_metadata(path);
        let name = metadata
            .name
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| path.file_name().map(ToOwned::to_owned))
            .context("skill path has no usable name")?;
        let relative = path
            .strip_prefix(root)
            .context("skill path escaped package root")?
            .to_string();
        let mut skill_diagnostics = metadata
            .diagnostics
            .iter()
            .map(|item| format!("{}: {}", item.path, item.message))
            .collect::<Vec<_>>();
        let content = std::fs::read_to_string(path.join("SKILL.md"))?;
        skill_diagnostics.extend(skill_behavior_diagnostics(&content));
        if !names.insert(name.clone()) {
            diagnostics.push(format!(
                "duplicate skill name `{name}` was not imported twice"
            ));
            continue;
        }
        result.push(SkillDefinition {
            name,
            description: metadata.description,
            path: relative,
            diagnostics: skill_diagnostics,
        });
    }
    Ok(result)
}

fn import_mcp(
    root: &Utf8Path,
    format: SourceFormat,
    manifest: &Map<String, Value>,
    diagnostics: &mut Vec<String>,
) -> Result<(Vec<McpServerDefinition>, Vec<String>)> {
    let default = match format {
        SourceFormat::Claude => ".mcp.json",
        SourceFormat::Codex | SourceFormat::Cursor => "mcp.json",
    };
    let declaration = manifest.get("mcpServers");
    let mut documents = Vec::new();
    if let Some(declaration) = declaration {
        collect_mcp_declaration(root, declaration, &mut documents)?;
    } else if root.join(default).is_file() {
        documents.push(read_json(&checked_component_path(root, default)?)?);
    }
    if format == SourceFormat::Codex && root.join("plugin.json").is_file() {
        // Portable package components are canonical; compatibility overlays do not supplement them.
        documents.clear();
        if root.join("mcp.json").is_file() {
            documents.push(read_json(&checked_component_path(root, "mcp.json")?)?);
        }
    }

    let mut servers = BTreeMap::<String, McpServerDefinition>::new();
    let mut conflicts = BTreeSet::new();
    for document in documents {
        let object = document
            .as_object()
            .context("MCP declaration must be an object")?;
        let map = object
            .get("mcpServers")
            .and_then(Value::as_object)
            .unwrap_or(object);
        for (name, value) in map {
            if conflicts.contains(name) {
                continue;
            }
            let server = normalize_server(name, value)?;
            if let Some(existing) = servers.get(name) {
                if existing != &server {
                    diagnostics.push(format!(
                        "conflicting MCP definitions for `{name}` were excluded and require explicit source cleanup"
                    ));
                    servers.remove(name);
                    conflicts.insert(name.clone());
                }
                continue;
            }
            servers.insert(name.clone(), server);
        }
    }
    Ok((
        servers.into_values().collect(),
        conflicts.into_iter().collect(),
    ))
}

fn collect_mcp_declaration(
    root: &Utf8Path,
    declaration: &Value,
    out: &mut Vec<Value>,
) -> Result<()> {
    match declaration {
        Value::String(path) => out.push(read_json(&checked_component_path(root, path)?)?),
        Value::Array(values) => {
            for value in values {
                collect_mcp_declaration(root, value, out)?;
            }
        }
        Value::Object(_) => out.push(declaration.clone()),
        _ => bail!("mcpServers must be a path, object, or array"),
    }
    Ok(())
}

fn normalize_server(name: &str, value: &Value) -> Result<McpServerDefinition> {
    let object = value
        .as_object()
        .with_context(|| format!("MCP server `{name}` must be an object"))?;
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let declared_type = object.get("type").and_then(Value::as_str);
    let transport = match declared_type {
        Some("stdio") => McpTransport::Stdio,
        Some("http" | "streamable-http" | "streamable_http") => McpTransport::Http,
        Some("sse") => McpTransport::Sse,
        Some("websocket" | "ws") => McpTransport::Websocket,
        Some(_) => McpTransport::Unknown,
        None if command.is_some() => McpTransport::Stdio,
        None if url.is_some() => McpTransport::Http,
        None => McpTransport::Unknown,
    };
    let args = object
        .get("args")
        .map(string_list)
        .transpose()?
        .unwrap_or_default();
    let env = string_map(object.get("env"), "env")?;
    let headers = string_map(object.get("headers"), "headers")?;
    let cwd = object
        .get("cwd")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let known = ["type", "command", "args", "url", "env", "headers", "cwd"];
    let extensions = object
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let server = McpServerDefinition {
        name: name.to_string(),
        transport,
        command,
        args,
        url,
        env,
        headers,
        cwd,
        extensions,
    };
    if let Some(location) = literal_credential_location(&server) {
        bail!("{}", literal_credential_error(name, &location));
    }
    Ok(server)
}

fn import_inputs(
    manifest: &Map<String, Value>,
    diagnostics: &mut Vec<String>,
) -> Result<Vec<InputDefinition>> {
    let Some(value) = manifest.get("inputs").or_else(|| manifest.get("variables")) else {
        return Ok(Vec::new());
    };
    let values = match value {
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_object)
            .filter_map(|object| {
                let name = object.get("name")?.as_str()?.to_string();
                Some((name, object))
            })
            .collect::<Vec<_>>(),
        Value::Object(values) => values
            .iter()
            .filter_map(|(name, value)| value.as_object().map(|object| (name.clone(), object)))
            .collect::<Vec<_>>(),
        _ => {
            diagnostics.push("input declarations were not an object or array".to_string());
            return Ok(Vec::new());
        }
    };
    values
        .into_iter()
        .map(|(name, object)| {
            let input_type = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("string")
                .to_string();
            let sensitive = object
                .get("sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(matches!(input_type.as_str(), "secret" | "password"))
                || is_sensitive_env_name(&name);
            if sensitive && object.contains_key("default") {
                bail!(
                    "sensitive input `{name}` declares a default value; remove the default and provide the credential through a secure runtime binding"
                );
            }
            let default = object.get("default").cloned();
            if default.as_ref().is_some_and(value_has_literal_credential) {
                bail!("input `{name}` contains a literal credential in its default; remove it and provide the credential through a secure runtime binding");
            }
            let constraints = object
                .iter()
                .filter(|(key, _)| {
                    !["name", "type", "required", "sensitive", "description", "default"]
                        .contains(&key.as_str())
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>();
            if constraints
                .iter()
                .any(|(key, value)| keyed_value_has_literal_credential(key, value))
            {
                bail!("input `{name}` contains a literal credential in its constraints; remove it and provide the credential through a secure runtime binding");
            }
            Ok(InputDefinition {
                name,
                sensitive,
                input_type,
                required: object
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                description: object
                    .get("description")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                default,
                constraints,
            })
        })
        .collect()
}

fn skill_behavior_diagnostics(content: &str) -> Vec<String> {
    let mut diagnostics = Vec::new();
    let mut lines = content.lines();
    if lines.next() == Some("---") {
        let yaml = lines
            .by_ref()
            .take_while(|line| *line != "---")
            .collect::<Vec<_>>();
        match serde_yaml::from_str::<serde_yaml::Value>(&yaml.join("\n")) {
            Ok(serde_yaml::Value::Mapping(mapping)) => {
                for (key, value) in mapping {
                    let Some(key) = key.as_str() else {
                        continue;
                    };
                    let unsupported = match key {
                        "context" => value.as_str() == Some("fork"),
                        "agent" | "hooks" => !value.is_null(),
                        _ => false,
                    };
                    if unsupported {
                        diagnostics.push(format!(
                            "source-specific frontmatter `{key}` is not supported for portable export"
                        ));
                    }
                }
            }
            Ok(_) => diagnostics.push(
                "skill frontmatter could not be inspected safely because it is not a mapping"
                    .to_string(),
            ),
            Err(_) => diagnostics.push(
                "skill frontmatter could not be inspected safely because it is invalid YAML"
                    .to_string(),
            ),
        }
    }
    if content.contains("mcp__") {
        diagnostics.push(
            "source-specific MCP tool reference `mcp__` is not supported for portable export"
                .to_string(),
        );
    }
    diagnostics
}

fn excluded_components(
    root: &Utf8Path,
    manifest: &Map<String, Value>,
) -> Result<Vec<ExcludedComponent>> {
    let candidates = [
        (
            "agents",
            "agent_prompt",
            "agent/subagent prompts are not supported",
        ),
        ("commands", "command", "legacy commands are deferred"),
        ("hooks", "hook", "hooks are deferred"),
        ("rules", "rule", "rules are deferred"),
        ("lsp", "lsp", "language servers are deferred"),
        ("workflows", "workflow", "workflows are deferred"),
        ("styles", "style", "output styles are deferred"),
        (
            "bin",
            "executable",
            "PATH injection and setup executables are deferred",
        ),
        ("channels", "channel", "channels are deferred"),
        ("themes", "theme", "themes are deferred"),
        ("monitors", "monitor", "monitors are deferred"),
        ("evals", "eval", "evaluation assets are deferred"),
    ];
    let mut result = Vec::new();
    for (path, kind, reason) in candidates {
        // A root skill may carry Codex display metadata, which is not an agent prompt.
        let skill_metadata_only = path == "agents"
            && root.join("SKILL.md").is_file()
            && root.join(path).is_dir()
            && std::fs::read_dir(root.join(path))?.all(|entry| {
                entry
                    .is_ok_and(|entry| entry.file_name() == "openai.yaml" && entry.path().is_file())
            });
        if root.join(path).exists() && !skill_metadata_only {
            result.push(ExcludedComponent {
                kind: kind.to_string(),
                location: path.to_string(),
                reason: reason.to_string(),
            });
        }
    }
    for (field, kind, reason) in [
        (
            "agents",
            "agent_prompt",
            "agent/subagent prompts are not supported",
        ),
        ("commands", "command", "legacy commands are deferred"),
        ("hooks", "hook", "hooks are deferred"),
        ("rules", "rule", "rules are deferred"),
        ("lspServers", "lsp", "language servers are deferred"),
        ("outputStyles", "style", "output styles are deferred"),
        ("workflows", "workflow", "workflows are deferred"),
        (
            "dependencies",
            "dependency",
            "dependencies are reported but not installed",
        ),
    ] {
        if manifest.contains_key(field)
            && !result.iter().any(|component| component.location == field)
        {
            result.push(ExcludedComponent {
                kind: kind.to_string(),
                location: field.to_string(),
                reason: reason.to_string(),
            });
        }
    }
    for (path, kind, reason) in [
        ("hooks.json", "hook", "hooks are deferred"),
        (
            ".app.json",
            "connection",
            "vendor connection IDs are not MCP endpoints",
        ),
    ] {
        if root.join(path).is_file() {
            result.push(ExcludedComponent {
                kind: kind.to_string(),
                location: path.to_string(),
                reason: reason.to_string(),
            });
        }
    }
    Ok(result)
}

fn checked_component_path(root: &Utf8Path, relative: &str) -> Result<Utf8PathBuf> {
    let relative = relative.strip_prefix("./").unwrap_or(relative);
    let relative = Utf8Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, camino::Utf8Component::ParentDir))
    {
        bail!("component path escapes package root: {relative}");
    }
    let candidate = root.join(relative);
    if !candidate.exists() {
        return Ok(candidate);
    }
    let canonical_root = Utf8PathBuf::from_path_buf(std::fs::canonicalize(root)?)
        .map_err(|path| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
    let canonical = Utf8PathBuf::from_path_buf(std::fs::canonicalize(&candidate)?)
        .map_err(|path| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
    if !canonical.starts_with(&canonical_root) {
        bail!("component path resolves outside package root: {relative}");
    }
    Ok(canonical)
}

fn sorted_dirs(root: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let mut result = std::fs::read_dir(root)?
        .map(|entry| {
            let entry = entry?;
            Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                std::io::Error::other(format!("non-UTF-8 path: {}", path.display()))
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    result.retain(|path| path.is_dir());
    result.sort();
    Ok(result)
}

fn string_list(value: &Value) -> Result<Vec<String>> {
    match value {
        Value::String(value) => Ok(vec![value.clone()]),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .context("expected a string path")
            })
            .collect(),
        _ => bail!("expected a string or array of strings"),
    }
}

fn string_map(value: Option<&Value>, field: &str) -> Result<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let object = value
        .as_object()
        .with_context(|| format!("MCP `{field}` must be an object"))?;
    object
        .iter()
        .map(|(key, value)| {
            let value = value
                .as_str()
                .with_context(|| format!("MCP `{field}.{key}` must be a string"))?;
            Ok((key.clone(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PluginSource;

    fn entry(format: SourceFormat) -> CatalogEntry {
        CatalogEntry {
            name: "demo".to_string(),
            description: None,
            source: PluginSource::Relative {
                path: "plugin".to_string(),
            },
            format,
            raw: Value::Null,
        }
    }

    #[test]
    fn imports_claude_skills_wrapped_and_direct_mcp_and_excludes_agents() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("skills/one")).unwrap();
        std::fs::create_dir_all(root.join("agents")).unwrap();
        std::fs::write(
            root.join("skills/one/SKILL.md"),
            "---\nname: one\ndescription: Demo\n---\n",
        )
        .unwrap();
        std::fs::write(root.join("agents/reviewer.md"), "prompt").unwrap();
        std::fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"local":{"command":"tool","args":["serve"]},"remote":{"type":"http","url":"https://example.test/mcp"}}}"#,
        )
        .unwrap();
        let plugin = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap();
        assert_eq!(plugin.skills.len(), 1);
        assert_eq!(plugin.mcp_servers.len(), 2);
        assert_eq!(plugin.import.excluded[0].kind, "agent_prompt");
    }

    #[test]
    fn cursor_explicit_skill_paths_replace_default_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        for path in ["skills/default", "custom/selected"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
            std::fs::write(
                root.join(path).join("SKILL.md"),
                format!("---\nname: {}\n---\n", path.replace('/', "-")),
            )
            .unwrap();
        }
        std::fs::create_dir_all(root.join(".cursor-plugin")).unwrap();
        std::fs::write(
            root.join(".cursor-plugin/plugin.json"),
            r#"{"name":"demo","skills":["custom"]}"#,
        )
        .unwrap();
        let plugin = import_plugin(&root, &entry(SourceFormat::Cursor), None, None).unwrap();
        assert_eq!(plugin.skills.len(), 1);
        assert_eq!(plugin.skills[0].name, "custom-selected");
    }

    #[test]
    fn codex_portable_components_take_precedence_over_overlay_fields() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        for path in ["skills/portable", "overlay/ignored"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
            std::fs::write(
                root.join(path).join("SKILL.md"),
                format!("---\nname: {}\n---\n", path.replace('/', "-")),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("plugin.json"),
            r#"{"name":"demo","skills":["overlay"],"mcpServers":{"ignored":{"command":"ignored"}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("mcp.json"),
            r#"{"mcpServers":{"portable":{"command":"portable"}}}"#,
        )
        .unwrap();
        let plugin = import_plugin(&root, &entry(SourceFormat::Codex), None, None).unwrap();
        assert_eq!(plugin.skills.len(), 1);
        assert_eq!(plugin.skills[0].name, "skills-portable");
        assert_eq!(plugin.mcp_servers[0].name, "portable");
    }

    #[test]
    fn claude_declared_skill_paths_are_additive_and_skill_sidecars_are_retained() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        for path in ["skills/default", "extra/declared"] {
            std::fs::create_dir_all(root.join(path).join("agents")).unwrap();
            std::fs::write(
                root.join(path).join("SKILL.md"),
                format!("---\nname: {}\n---\n", path.replace('/', "-")),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("skills/default/agents/openai.yaml"),
            "policy:\n  allow_implicit_invocation: true\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/plugin.json"),
            r#"{"name":"demo","skills":["extra"]}"#,
        )
        .unwrap();
        let plugin = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap();
        assert_eq!(
            plugin
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["skills-default", "extra-declared"]
        );
        assert!(plugin.import.excluded.is_empty());
    }

    #[test]
    fn claude_marketplace_root_skill_override_replaces_default_scan() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        for path in ["skills/default", "selected/only"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
            std::fs::write(
                root.join(path).join("SKILL.md"),
                format!("---\nname: {}\n---\n", path.replace('/', "-")),
            )
            .unwrap();
        }
        let mut catalog_entry = entry(SourceFormat::Claude);
        catalog_entry.source = PluginSource::Relative {
            path: ".".to_string(),
        };
        catalog_entry.raw = serde_json::json!({"skills": ["selected"]});
        let plugin = import_plugin(&root, &catalog_entry, None, None).unwrap();
        assert_eq!(plugin.skills.len(), 1);
        assert_eq!(plugin.skills[0].name, "selected-only");
    }

    #[test]
    fn imports_recorded_context7_and_playwright_mcp_shapes() {
        let fixtures = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/anthropic-claude-plugins-official-c447c320");
        let context7 = import_plugin(
            &fixtures.join("context7"),
            &entry(SourceFormat::Claude),
            Some("c447c3207a425bc4e2a0d068435f64b0477ae981".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(context7.mcp_servers[0].name, "context7");
        assert_eq!(context7.mcp_servers[0].transport, McpTransport::Http);
        assert_eq!(
            context7.mcp_servers[0].headers["Authorization"],
            "${CONTEXT7_API_KEY:-}"
        );

        let playwright = import_plugin(
            &fixtures.join("playwright"),
            &entry(SourceFormat::Claude),
            Some("c447c3207a425bc4e2a0d068435f64b0477ae981".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(playwright.mcp_servers[0].name, "playwright");
        assert_eq!(playwright.mcp_servers[0].transport, McpTransport::Stdio);
        assert_eq!(playwright.mcp_servers[0].args, ["@playwright/mcp@latest"]);
    }

    #[test]
    fn quoted_source_specific_skill_frontmatter_is_detected_semantically() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("skills/unsafe")).unwrap();
        std::fs::write(
            root.join("skills/unsafe/SKILL.md"),
            "---\nname: unsafe\ncontext: \"fork\"\n\"hooks\": {}\nagent: reviewer\n---\nPrompt\n",
        )
        .unwrap();

        let plugin = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap();

        assert_eq!(plugin.skills.len(), 1);
        let diagnostics = plugin.skills[0].diagnostics.join("\n");
        assert!(diagnostics.contains("`context`"), "{diagnostics}");
        assert!(diagnostics.contains("`hooks`"), "{diagnostics}");
        assert!(diagnostics.contains("`agent`"), "{diagnostics}");
    }

    #[test]
    fn conflicting_mcp_definitions_are_excluded_instead_of_selecting_one() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/plugin.json"),
            r#"{"name":"demo","mcpServers":[{"same":{"command":"one"}},{"same":{"command":"two"}}]}"#,
        )
        .unwrap();

        let plugin = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap();

        assert!(plugin.mcp_servers.is_empty());
        assert!(plugin
            .import
            .excluded
            .iter()
            .any(|component| component.kind == "mcp_conflict" && component.location == "same"));
        assert!(plugin
            .import
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("conflicting MCP definitions")));
    }

    #[test]
    fn conflict_detection_uses_operational_values_hidden_by_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/plugin.json"),
            r#"{"name":"demo","mcpServers":[{"same":{"command":"run","env":{"CUSTOM":"one"}}},{"same":{"command":"run","env":{"CUSTOM":"two"}}}]}"#,
        )
        .unwrap();
        let plugin = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap();
        assert!(plugin.mcp_servers.is_empty());
        assert!(plugin
            .import
            .excluded
            .iter()
            .any(|item| item.kind == "mcp_conflict"));
    }

    #[test]
    fn literal_credentials_are_rejected_without_echoing_the_value() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"private":{"type":"http","url":"https://example.test","headers":{"Authorization":"Bearer top-secret"}}}}"#,
        )
        .unwrap();

        let error = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("literal credential"), "{message}");
        assert!(!message.contains("top-secret"), "{message}");
    }

    #[test]
    fn nonempty_credential_fallbacks_are_rejected_without_echoing_the_value() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"private":{"type":"http","url":"https://example.test","headers":{"Authorization":"Bearer ${TOKEN:-top-secret}"}}}}"#,
        )
        .unwrap();

        let error = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("literal credential"), "{message}");
        assert!(!message.contains("top-secret"), "{message}");
    }

    #[test]
    fn sensitive_input_defaults_are_rejected_without_echoing_the_value() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/plugin.json"),
            r#"{"name":"demo","inputs":[{"name":"token","type":"password","default":"top-secret"}]}"#,
        )
        .unwrap();

        let error = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("sensitive input `token`"), "{message}");
        assert!(!message.contains("top-secret"), "{message}");
    }

    #[test]
    fn credential_bearing_urls_are_rejected_without_echoing_the_value() {
        for url in [
            "https://user:top-secret@example.test/mcp",
            "https://example.test/mcp?api_key=top-secret",
            "https://example.test/mcp?sig=top-secret",
            "https://example.test/mcp?%73ig=top-secret",
            "https://example.test/mcp?%74oken=top-secret",
            "https://example.test/token/top-secret/mcp",
            "https://example.test/mcp#top-secret",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            std::fs::write(
                root.join(".mcp.json"),
                serde_json::json!({"mcpServers": {"private": {"type": "http", "url": url}}})
                    .to_string(),
            )
            .unwrap();
            let message = import_plugin(&root, &entry(SourceFormat::Claude), None, None)
                .unwrap_err()
                .to_string();
            assert!(
                message.contains("literal credential in its URL"),
                "{message}"
            );
            assert!(!message.contains("top-secret"), "{message}");
        }
    }

    #[test]
    fn symbolic_url_credentials_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let url = "https://example.test/mcp?api_key=${env:TOKEN}";
        std::fs::write(
            root.join(".mcp.json"),
            serde_json::json!({"mcpServers": {"private": {"type": "http", "url": url}}})
                .to_string(),
        )
        .unwrap();
        let plugin = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap();
        assert_eq!(plugin.mcp_servers[0].url.as_deref(), Some(url));
    }

    #[test]
    fn embedded_and_nested_credentials_are_rejected_without_echoing_values() {
        for (server, location) in [
            (
                serde_json::json!({"command": "run https://user:top-secret@example.test/mcp"}),
                "command",
            ),
            (
                serde_json::json!({"command": "run", "args": ["--endpoint=https://example.test/mcp?sig=top-secret"]}),
                "argument 0",
            ),
            (
                serde_json::json!({"command": "run", "vendor": {"nested": [{"apiKey": "top-secret"}]}}),
                "extension data",
            ),
            (
                serde_json::json!({"command": "run", "vendor": {"endpoint": "https://example.test/mcp?sig=top-secret"}}),
                "extension data",
            ),
            (
                serde_json::json!({"command": "run", "env": {"ENDPOINT": "https://example.test/mcp?sig=top-secret"}}),
                "environment variable `ENDPOINT`",
            ),
            (
                serde_json::json!({"command": "run", "headers": {"X-Endpoint": "https://example.test/mcp?sig=top-secret"}}),
                "header `X-Endpoint`",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            std::fs::write(
                root.join(".mcp.json"),
                serde_json::json!({"mcpServers": {"private": server}}).to_string(),
            )
            .unwrap();
            let error = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap_err();
            let message = format!("{error:#}\n{error:?}");
            assert!(message.contains(location), "{message}");
            assert!(!message.contains("top-secret"), "{message}");
        }
    }

    #[test]
    fn nested_input_credentials_are_rejected_without_echoing_values() {
        for input in [
            serde_json::json!({"name": "public", "type": "string", "default": {"endpoint": "https://example.test/mcp?sig=top-secret"}}),
            serde_json::json!({"name": "public", "type": "string", "options": [{"token": "top-secret"}]}),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
            std::fs::write(
                root.join(".claude-plugin/plugin.json"),
                serde_json::json!({"name": "demo", "inputs": [input]}).to_string(),
            )
            .unwrap();
            let error = import_plugin(&root, &entry(SourceFormat::Claude), None, None).unwrap_err();
            let message = format!("{error:#}\n{error:?}");
            assert!(message.contains("literal credential"), "{message}");
            assert!(!message.contains("top-secret"), "{message}");
        }
    }
}

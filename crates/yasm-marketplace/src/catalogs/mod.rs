use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;

use crate::model::{CatalogEntry, PluginSource, SourceFormat};

const CANDIDATES: &[(SourceFormat, &str)] = &[
    (SourceFormat::Codex, ".agents/plugins/marketplace.json"),
    (SourceFormat::Claude, ".claude-plugin/marketplace.json"),
    (SourceFormat::Cursor, ".cursor-plugin/marketplace.json"),
];

pub struct ParsedCatalog {
    pub name: String,
    pub format: SourceFormat,
    pub path: String,
    pub entries: Vec<CatalogEntry>,
}

pub fn parse_catalog(root: &Utf8Path, requested: Option<SourceFormat>) -> Result<ParsedCatalog> {
    let found = CANDIDATES
        .iter()
        .filter(|(format, path)| {
            requested.is_none_or(|wanted| wanted == *format) && root.join(path).is_file()
        })
        .copied()
        .collect::<Vec<_>>();

    if found.is_empty() {
        let expected = CANDIDATES
            .iter()
            .filter(|(format, _)| requested.is_none_or(|wanted| wanted == *format))
            .map(|(_, path)| *path)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("no supported marketplace catalog found; expected one of: {expected}");
    }
    if found.len() > 1 {
        let formats = found
            .iter()
            .map(|(format, _)| format.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("multiple marketplace formats found ({formats}); pass --format <{formats}>");
    }

    let (format, relative_path) = found[0];
    let path = root.join(relative_path);
    let raw: Value = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("failed to read catalog {path}"))?,
    )
    .with_context(|| format!("failed to parse catalog {path}"))?;
    let object = raw
        .as_object()
        .context("marketplace catalog must be a JSON object")?;
    let name = required_string(object.get("name"), "marketplace name")?;
    let plugins = object
        .get("plugins")
        .and_then(Value::as_array)
        .context("marketplace catalog requires a `plugins` array")?;
    let mut entries = Vec::with_capacity(plugins.len());
    for plugin in plugins {
        let entry = plugin
            .as_object()
            .context("marketplace plugin entry must be an object")?;
        let entry_name = required_string(entry.get("name"), "plugin name")?;
        validate_identifier(&entry_name, "plugin name")?;
        let source = parse_source(
            entry
                .get("source")
                .context("marketplace plugin entry requires `source`")?,
        )
        .with_context(|| format!("invalid source for plugin `{entry_name}`"))?;
        entries.push(CatalogEntry {
            name: entry_name,
            description: entry
                .get("description")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            source,
            format,
            raw: plugin.clone(),
        });
    }
    Ok(ParsedCatalog {
        name,
        format,
        path: relative_path.to_string(),
        entries,
    })
}

fn required_string(value: Option<&Value>, field: &str) -> Result<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .with_context(|| format!("{field} must be a non-empty string"))
}

fn validate_identifier(value: &str, field: &str) -> Result<()> {
    if value == "."
        || value == ".."
        || value
            .chars()
            .any(|character| matches!(character, '/' | '\\' | '@'))
    {
        bail!("{field} contains a reserved path or qualifier character: `{value}`");
    }
    Ok(())
}

fn parse_source(value: &Value) -> Result<PluginSource> {
    if let Some(path) = value.as_str() {
        validate_relative(path)?;
        return Ok(PluginSource::Relative {
            path: path.to_string(),
        });
    }
    let object = value
        .as_object()
        .context("source must be a path or object")?;
    let kind = object
        .get("source")
        .or_else(|| object.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("git");
    match kind {
        "relative" | "directory" | "local" => {
            let path = required_string(object.get("path"), "relative source path")?;
            validate_relative(&path)?;
            Ok(PluginSource::Relative { path })
        }
        "git" | "git-subdir" | "url" | "github" => {
            let mut url = object
                .get("url")
                .or_else(|| object.get("repository"))
                .or_else(|| object.get("repo"))
                .and_then(Value::as_str)
                .context("Git source requires `url`")?
                .to_string();
            if !url.contains("://") && url.split('/').count() == 2 {
                url = format!("https://github.com/{url}.git");
            }
            if !url.starts_with("https://github.com/") {
                let displayed = crate::secrets::redact_credential_urls_in_text(&url);
                bail!("unsupported Git source URL `{displayed}`; only HTTPS GitHub repositories are supported");
            }
            let path = object
                .get("path")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if let Some(path) = &path {
                validate_relative(path)?;
            }
            Ok(PluginSource::Git {
                url,
                path,
                r#ref: object
                    .get("ref")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                sha: object
                    .get("sha")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            })
        }
        other => bail!("unsupported marketplace source kind `{other}`"),
    }
}

pub fn checked_relative(root: &Utf8Path, relative: &str) -> Result<Utf8PathBuf> {
    validate_relative(relative)?;
    let candidate = root.join(relative);
    let canonical_root = Utf8PathBuf::from_path_buf(std::fs::canonicalize(root)?)
        .map_err(|path| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
    let canonical = Utf8PathBuf::from_path_buf(
        std::fs::canonicalize(&candidate)
            .with_context(|| format!("plugin source path does not exist: {relative}"))?,
    )
    .map_err(|path| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
    if !canonical.starts_with(&canonical_root) {
        bail!("plugin source path escapes the marketplace repository: {relative}");
    }
    if !canonical.is_dir() {
        bail!("plugin source path is not a directory: {relative}");
    }
    Ok(canonical)
}

fn validate_relative(path: &str) -> Result<()> {
    let path = path.strip_prefix("./").unwrap_or(path);
    let path = Utf8Path::new(path);
    if path.as_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, camino::Utf8Component::ParentDir))
    {
        bail!("source path must stay within its repository: {path}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_and_git_subdir_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/marketplace.json"),
            r#"{"name":"demo","plugins":[{"name":"local","source":"./plugins/a"},{"name":"remote","source":{"source":"git-subdir","url":"https://github.com/acme/tools.git","path":"plugin","ref":"main","sha":"abc"}}]}"#,
        )
        .unwrap();
        let catalog = parse_catalog(&root, None).unwrap();
        assert_eq!(catalog.name, "demo");
        assert_eq!(catalog.entries.len(), 2);
        assert!(matches!(
            catalog.entries[0].source,
            PluginSource::Relative { .. }
        ));
        assert!(matches!(
            catalog.entries[1].source,
            PluginSource::Git { .. }
        ));
    }

    #[test]
    fn parses_codex_local_source_objects_as_relative_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".agents/plugins")).unwrap();
        std::fs::write(
            root.join(".agents/plugins/marketplace.json"),
            r#"{"name":"openai-plugins","plugins":[{"name":"linear","source":{"source":"local","path":"./plugins/linear"}}]}"#,
        )
        .unwrap();

        let catalog = parse_catalog(&root, Some(SourceFormat::Codex)).unwrap();
        assert!(matches!(
            &catalog.entries[0].source,
            PluginSource::Relative { path } if path == "./plugins/linear"
        ));
    }

    #[test]
    fn multiple_catalog_formats_require_an_explicit_choice() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        for path in [
            ".agents/plugins/marketplace.json",
            ".cursor-plugin/marketplace.json",
        ] {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(
                root.join(path),
                r#"{"name":"demo","plugins":[{"name":"one","source":"plugin"}]}"#,
            )
            .unwrap();
        }
        let error = parse_catalog(&root, None).err().unwrap();
        assert!(error.to_string().contains("pass --format"));
        assert_eq!(
            parse_catalog(&root, Some(SourceFormat::Cursor))
                .unwrap()
                .format,
            SourceFormat::Cursor
        );
    }

    #[test]
    fn credential_bearing_source_url_error_does_not_echo_secret() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/marketplace.json"),
            r#"{"name":"demo","plugins":[{"name":"remote","source":{"source":"git","url":"https://user:top-secret@github.com/example/repo.git"}}]}"#,
        )
        .unwrap();
        let error = parse_catalog(&root, None)
            .err()
            .expect("credential URL rejected");
        let rendered = format!("{error:#}\n{error:?}");
        assert!(
            rendered.contains("unsupported Git source URL"),
            "{rendered}"
        );
        assert!(!rendered.contains("top-secret"), "{rendered}");
    }
}

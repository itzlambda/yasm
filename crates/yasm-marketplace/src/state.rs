use std::collections::BTreeMap;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::model::{
    CatalogEntry, InputDefinition, InstallationRecord, MarketplaceRecord, McpServerDefinition,
    PluginDefinition, PluginSource,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct RegistryState {
    #[serde(deserialize_with = "deserialize_state_version")]
    pub version: u32,
    pub marketplaces: BTreeMap<String, MarketplaceRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallationState {
    #[serde(deserialize_with = "deserialize_state_version")]
    pub version: u32,
    /// Durable commit marker, including operations whose installation record is unchanged.
    pub generation: u64,
    pub plugins: BTreeMap<String, InstallationRecord>,
}

const STATE_VERSION: u32 = 1;

impl Default for RegistryState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            marketplaces: BTreeMap::new(),
        }
    }
}

impl Default for InstallationState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            generation: 0,
            plugins: BTreeMap::new(),
        }
    }
}

fn deserialize_state_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let version = u32::deserialize(deserializer)?;
    if version != STATE_VERSION {
        return Err(serde::de::Error::custom(format!(
            "unsupported marketplace state version {version}; expected version {STATE_VERSION}"
        )));
    }
    Ok(version)
}

#[derive(Debug, Clone)]
pub struct MarketplacePaths {
    pub root: Utf8PathBuf,
    pub cache: Utf8PathBuf,
}

impl MarketplacePaths {
    pub fn new(data_dir: &Utf8Path, cache_dir: &Utf8Path) -> Self {
        Self {
            root: data_dir.join("marketplaces"),
            cache: cache_dir.join("marketplaces"),
        }
    }

    pub fn registry(&self) -> Utf8PathBuf {
        self.root.join("registry.json")
    }

    pub fn installations(&self) -> Utf8PathBuf {
        self.root.join("installations.json")
    }

    pub fn packages(&self) -> Utf8PathBuf {
        self.root.join("packages")
    }

    pub fn data(&self) -> Utf8PathBuf {
        self.root.join("data")
    }

    pub fn operations(&self) -> Utf8PathBuf {
        self.root.join("operations")
    }
}

pub fn load_registry(paths: &MarketplacePaths) -> Result<RegistryState> {
    load_json(&paths.registry())
}

pub fn save_registry(paths: &MarketplacePaths, state: &RegistryState) -> Result<()> {
    let marketplaces = state
        .marketplaces
        .iter()
        .map(|(name, record)| (name, RawMarketplaceRecord::from(record)))
        .collect::<BTreeMap<_, _>>();
    save_json(
        &paths.registry(),
        &RawRegistryState {
            version: state.version,
            marketplaces,
        },
    )
}

pub fn load_installations(paths: &MarketplacePaths) -> Result<InstallationState> {
    load_json(&paths.installations())
}

pub fn save_installations(paths: &MarketplacePaths, state: &InstallationState) -> Result<()> {
    let plugins = state
        .plugins
        .iter()
        .map(|(id, record)| (id, RawInstallationRecord::from(record)))
        .collect::<BTreeMap<_, _>>();
    save_json(
        &paths.installations(),
        &RawInstallationState {
            version: state.version,
            generation: state.generation,
            plugins,
        },
    )
}

// Only state persistence uses these raw views. Ordinary Serialize and Debug on
// marketplace models are diagnostic interfaces and redact retained secrets.
#[derive(Serialize)]
struct RawRegistryState<'a> {
    version: u32,
    marketplaces: BTreeMap<&'a String, RawMarketplaceRecord<'a>>,
}

#[derive(Serialize)]
struct RawMarketplaceRecord<'a> {
    name: &'a str,
    source: &'a yasm_core::SourceSpec,
    format: &'a crate::model::SourceFormat,
    catalog_path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_revision: &'a Option<String>,
    entries: Vec<RawCatalogEntry<'a>>,
}

impl<'a> From<&'a MarketplaceRecord> for RawMarketplaceRecord<'a> {
    fn from(record: &'a MarketplaceRecord) -> Self {
        Self {
            name: &record.name,
            source: &record.source,
            format: &record.format,
            catalog_path: &record.catalog_path,
            resolved_revision: &record.resolved_revision,
            entries: record.entries.iter().map(RawCatalogEntry::from).collect(),
        }
    }
}

#[derive(Serialize)]
struct RawCatalogEntry<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: &'a Option<String>,
    source: RawPluginSource<'a>,
    format: &'a crate::model::SourceFormat,
    raw: &'a serde_json::Value,
}

impl<'a> From<&'a CatalogEntry> for RawCatalogEntry<'a> {
    fn from(entry: &'a CatalogEntry) -> Self {
        Self {
            name: &entry.name,
            description: &entry.description,
            source: RawPluginSource::from(&entry.source),
            format: &entry.format,
            raw: &entry.raw,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RawPluginSource<'a> {
    Relative {
        path: &'a str,
    },
    Git {
        url: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        r#ref: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: &'a Option<String>,
    },
}

impl<'a> From<&'a PluginSource> for RawPluginSource<'a> {
    fn from(source: &'a PluginSource) -> Self {
        match source {
            PluginSource::Relative { path } => Self::Relative { path },
            PluginSource::Git {
                url,
                path,
                r#ref,
                sha,
            } => Self::Git {
                url,
                path,
                r#ref,
                sha,
            },
        }
    }
}

#[derive(Serialize)]
struct RawInstallationState<'a> {
    version: u32,
    generation: u64,
    plugins: BTreeMap<&'a String, RawInstallationRecord<'a>>,
}

#[derive(Serialize)]
pub(crate) struct RawInstallationRecord<'a> {
    id: &'a str,
    marketplace: &'a str,
    entry_name: &'a str,
    marketplace_source: &'a yasm_core::SourceSpec,
    catalog_format: &'a crate::model::SourceFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog_revision: &'a Option<String>,
    source: RawPluginSource<'a>,
    catalog_entry: RawCatalogEntry<'a>,
    snapshot: &'a Utf8PathBuf,
    definition: RawPluginDefinition<'a>,
    enabled: &'a std::collections::BTreeSet<crate::model::TargetAgent>,
    outputs: &'a Vec<crate::model::OutputReceipt>,
}

impl<'a> From<&'a InstallationRecord> for RawInstallationRecord<'a> {
    fn from(record: &'a InstallationRecord) -> Self {
        Self {
            id: &record.id,
            marketplace: &record.marketplace,
            entry_name: &record.entry_name,
            marketplace_source: &record.marketplace_source,
            catalog_format: &record.catalog_format,
            catalog_revision: &record.catalog_revision,
            source: RawPluginSource::from(&record.source),
            catalog_entry: RawCatalogEntry::from(&record.catalog_entry),
            snapshot: &record.snapshot,
            definition: RawPluginDefinition::from(&record.definition),
            enabled: &record.enabled,
            outputs: &record.outputs,
        }
    }
}

#[derive(Serialize)]
struct RawPluginDefinition<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: &'a Option<String>,
    skills: &'a Vec<crate::model::SkillDefinition>,
    #[serde(rename = "mcpServers")]
    mcp_servers: Vec<RawMcpServer<'a>>,
    inputs: Vec<RawInput<'a>>,
    import: &'a crate::model::ImportRecord,
}

impl<'a> From<&'a PluginDefinition> for RawPluginDefinition<'a> {
    fn from(definition: &'a PluginDefinition) -> Self {
        Self {
            name: &definition.name,
            description: &definition.description,
            version: &definition.version,
            skills: &definition.skills,
            mcp_servers: definition
                .mcp_servers
                .iter()
                .map(RawMcpServer::from)
                .collect(),
            inputs: definition.inputs.iter().map(RawInput::from).collect(),
            import: &definition.import,
        }
    }
}

#[derive(Serialize)]
struct RawMcpServer<'a> {
    name: &'a str,
    transport: &'a crate::model::McpTransport,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: &'a Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: &'a Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: &'a Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: &'a BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    headers: &'a BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: &'a Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    extensions: &'a BTreeMap<String, serde_json::Value>,
}

impl<'a> From<&'a McpServerDefinition> for RawMcpServer<'a> {
    fn from(server: &'a McpServerDefinition) -> Self {
        Self {
            name: &server.name,
            transport: &server.transport,
            command: &server.command,
            args: &server.args,
            url: &server.url,
            env: &server.env,
            headers: &server.headers,
            cwd: &server.cwd,
            extensions: &server.extensions,
        }
    }
}

#[derive(Serialize)]
struct RawInput<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    input_type: &'a str,
    required: bool,
    sensitive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: &'a Option<serde_json::Value>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    constraints: &'a BTreeMap<String, serde_json::Value>,
}

impl<'a> From<&'a InputDefinition> for RawInput<'a> {
    fn from(input: &'a InputDefinition) -> Self {
        Self {
            name: &input.name,
            input_type: &input.input_type,
            required: input.required,
            sensitive: input.sensitive,
            description: &input.description,
            default: &input.default,
            constraints: &input.constraints,
        }
    }
}

fn load_json<T>(path: &Utf8Path) -> Result<T>
where
    T: DeserializeOwned + Default,
{
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse marketplace state {path}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {path}")),
    }
}

pub(crate) fn save_json<T: Serialize>(path: &Utf8Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("failed to serialize marketplace state {path}"))?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

pub(crate) fn write_atomic(path: &Utf8Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("output path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create output directory {parent}"))?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to stage output in {parent}"))?;
    temp.write_all(bytes)?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to publish output {path}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_requires_the_current_version_and_fields() {
        for version in [0, STATE_VERSION + 1, 999] {
            for value in [
                json!({"version": version, "marketplaces": {}}),
                json!({"version": version, "generation": 0, "plugins": {}}),
            ] {
                let error = if value.get("plugins").is_some() {
                    serde_json::from_value::<InstallationState>(value).unwrap_err()
                } else {
                    serde_json::from_value::<RegistryState>(value).unwrap_err()
                };
                assert!(error
                    .to_string()
                    .contains("unsupported marketplace state version"));
            }
        }
        let error = serde_json::from_value::<InstallationState>(json!({
            "version": STATE_VERSION, "plugins": {}
        }))
        .unwrap_err();
        assert!(error.to_string().contains("missing field `generation`"));
    }

    #[test]
    fn missing_state_starts_at_the_current_version_and_roundtrips() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let paths = MarketplacePaths::new(&root.join("data"), &root.join("cache"));
        let registry = load_registry(&paths).unwrap();
        let installations = load_installations(&paths).unwrap();
        assert_eq!(registry.version, STATE_VERSION);
        assert_eq!(installations.version, STATE_VERSION);
        save_registry(&paths, &registry).unwrap();
        save_installations(&paths, &installations).unwrap();
        assert_eq!(load_registry(&paths).unwrap().version, STATE_VERSION);
        assert_eq!(load_installations(&paths).unwrap().version, STATE_VERSION);
    }
}

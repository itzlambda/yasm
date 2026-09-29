use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use yasm_core::SourceSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    Codex,
    Claude,
    Cursor,
}

impl SourceFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum TargetAgent {
    Codex,
    Claude,
    Cursor,
}

impl TargetAgent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginSource {
    Relative {
        path: String,
    },
    Git {
        url: String,
        path: Option<String>,
        r#ref: Option<String>,
        sha: Option<String>,
    },
}

#[derive(Clone, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub description: Option<String>,
    pub source: PluginSource,
    pub format: SourceFormat,
    pub raw: Value,
}

impl CatalogEntry {
    pub fn retained_for_installation(&self) -> Self {
        let mut retained = self.clone();
        retained.raw = self
            .raw
            .get("skills")
            .map(|skills| serde_json::json!({"skills": skills}))
            .unwrap_or(Value::Null);
        retained
    }
}

#[derive(Clone, Deserialize)]
pub struct MarketplaceRecord {
    pub name: String,
    pub source: SourceSpec,
    pub format: SourceFormat,
    pub catalog_path: String,
    pub resolved_revision: Option<String>,
    pub entries: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
    Websocket,
    Unknown,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct McpServerDefinition {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Deserialize)]
pub struct InputDefinition {
    pub name: String,
    #[serde(rename = "type")]
    pub input_type: String,
    pub required: bool,
    pub sensitive: bool,
    pub description: Option<String>,
    pub default: Option<Value>,
    #[serde(default)]
    pub constraints: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludedComponent {
    pub kind: String,
    pub location: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRecord {
    pub format: SourceFormat,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifests: Vec<String>,
    pub marketplace_entry: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_revision: Option<String>,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<ExcludedComponent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub skills: Vec<SkillDefinition>,
    #[serde(rename = "mcpServers")]
    pub mcp_servers: Vec<McpServerDefinition>,
    pub inputs: Vec<InputDefinition>,
    pub import: ImportRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputReceipt {
    pub target: TargetAgent,
    pub skill_links: Vec<Utf8PathBuf>,
    pub skill_sources: BTreeMap<String, String>,
    pub skill_exports: BTreeMap<String, Utf8PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_config: Option<Utf8PathBuf>,
    pub mcp_names: Vec<String>,
    pub mcp_sources: BTreeMap<String, String>,
}

#[derive(Clone, Deserialize)]
pub struct InstallationRecord {
    pub id: String,
    pub marketplace: String,
    pub entry_name: String,
    pub marketplace_source: SourceSpec,
    pub catalog_format: SourceFormat,
    pub catalog_revision: Option<String>,
    pub source: PluginSource,
    pub catalog_entry: CatalogEntry,
    pub snapshot: Utf8PathBuf,
    pub definition: PluginDefinition,
    pub enabled: BTreeSet<TargetAgent>,
    pub outputs: Vec<OutputReceipt>,
}

impl InstallationRecord {
    pub fn storage_key(&self) -> String {
        plugin_storage_key(&self.id)
    }
}

macro_rules! diagnostic_traits {
    ($model:ty, $view:ident) => {
        impl std::fmt::Debug for $model {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(&$view::from(self), formatter)
            }
        }

        impl Serialize for $model {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                $view::from(self).serialize(serializer)
            }
        }
    };
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DiagnosticPluginSource<'a> {
    Relative {
        path: &'a str,
    },
    Git {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        r#ref: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: &'a Option<String>,
    },
}

impl<'a> From<&'a PluginSource> for DiagnosticPluginSource<'a> {
    fn from(source: &'a PluginSource) -> Self {
        match source {
            PluginSource::Relative { path } => Self::Relative { path },
            PluginSource::Git {
                url,
                path,
                r#ref,
                sha,
            } => Self::Git {
                url: crate::secrets::redact_credential_urls_in_text(url),
                path,
                r#ref,
                sha,
            },
        }
    }
}

diagnostic_traits!(PluginSource, DiagnosticPluginSource);

#[derive(Debug, Serialize)]
struct DiagnosticCatalogEntry<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: &'a Option<String>,
    source: &'a PluginSource,
    format: &'a SourceFormat,
    raw: &'static str,
}

impl<'a> From<&'a CatalogEntry> for DiagnosticCatalogEntry<'a> {
    fn from(entry: &'a CatalogEntry) -> Self {
        Self {
            name: &entry.name,
            description: &entry.description,
            source: &entry.source,
            format: &entry.format,
            raw: REDACTED,
        }
    }
}

diagnostic_traits!(CatalogEntry, DiagnosticCatalogEntry);

#[derive(Debug, Serialize)]
struct DiagnosticMarketplaceRecord<'a> {
    name: &'a str,
    source: SourceSpec,
    format: &'a SourceFormat,
    catalog_path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_revision: &'a Option<String>,
    entries: &'a [CatalogEntry],
}

impl<'a> From<&'a MarketplaceRecord> for DiagnosticMarketplaceRecord<'a> {
    fn from(record: &'a MarketplaceRecord) -> Self {
        Self {
            name: &record.name,
            source: redacted_source_spec(&record.source),
            format: &record.format,
            catalog_path: &record.catalog_path,
            resolved_revision: &record.resolved_revision,
            entries: &record.entries,
        }
    }
}

diagnostic_traits!(MarketplaceRecord, DiagnosticMarketplaceRecord);

#[derive(Debug, Serialize)]
struct DiagnosticMcpServer<'a> {
    name: &'a str,
    transport: &'a McpTransport,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    extensions: BTreeMap<String, Value>,
}

impl<'a> From<&'a McpServerDefinition> for DiagnosticMcpServer<'a> {
    fn from(server: &'a McpServerDefinition) -> Self {
        Self {
            name: &server.name,
            transport: &server.transport,
            command: server
                .command
                .as_deref()
                .map(crate::secrets::redact_credential_urls_in_text),
            args: redact_texts(&server.args),
            url: server.url.as_deref().map(redact_url),
            env: redact_named_values(&server.env),
            headers: redact_named_values(&server.headers),
            cwd: server
                .cwd
                .as_deref()
                .map(crate::secrets::redact_credential_urls_in_text),
            extensions: redact_values(&server.extensions),
        }
    }
}

diagnostic_traits!(McpServerDefinition, DiagnosticMcpServer);

#[derive(Debug, Serialize)]
struct DiagnosticInput<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    input_type: &'a str,
    required: bool,
    sensitive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<Value>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    constraints: BTreeMap<String, Value>,
}

impl<'a> From<&'a InputDefinition> for DiagnosticInput<'a> {
    fn from(input: &'a InputDefinition) -> Self {
        Self {
            name: &input.name,
            input_type: &input.input_type,
            required: input.required,
            sensitive: input.sensitive,
            description: &input.description,
            default: redact_default(&input.default, input.sensitive),
            constraints: redact_values(&input.constraints),
        }
    }
}

diagnostic_traits!(InputDefinition, DiagnosticInput);

#[derive(Debug, Serialize)]
struct DiagnosticInstallationRecord<'a> {
    id: &'a str,
    marketplace: &'a str,
    entry_name: &'a str,
    marketplace_source: SourceSpec,
    catalog_format: &'a SourceFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog_revision: &'a Option<String>,
    source: &'a PluginSource,
    catalog_entry: &'a CatalogEntry,
    snapshot: &'a Utf8PathBuf,
    definition: &'a PluginDefinition,
    enabled: &'a BTreeSet<TargetAgent>,
    outputs: &'a [OutputReceipt],
}

impl<'a> From<&'a InstallationRecord> for DiagnosticInstallationRecord<'a> {
    fn from(record: &'a InstallationRecord) -> Self {
        Self {
            id: &record.id,
            marketplace: &record.marketplace,
            entry_name: &record.entry_name,
            marketplace_source: redacted_source_spec(&record.marketplace_source),
            catalog_format: &record.catalog_format,
            catalog_revision: &record.catalog_revision,
            source: &record.source,
            catalog_entry: &record.catalog_entry,
            snapshot: &record.snapshot,
            definition: &record.definition,
            enabled: &record.enabled,
            outputs: &record.outputs,
        }
    }
}

diagnostic_traits!(InstallationRecord, DiagnosticInstallationRecord);

fn redacted_source_spec(source: &SourceSpec) -> SourceSpec {
    let mut source = source.clone();
    source.path = crate::secrets::redact_credential_urls_in_text(&source.path);
    source
}

const REDACTED: &str = "[redacted]";

fn redact_default(value: &Option<Value>, sensitive: bool) -> Option<Value> {
    value.as_ref().map(|value| {
        if sensitive {
            Value::String(REDACTED.to_string())
        } else {
            crate::secrets::redact_nested_value(value)
        }
    })
}

fn redact_texts(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| crate::secrets::redact_credential_urls_in_text(value))
        .collect()
}

fn redact_values(values: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    values
        .keys()
        .map(|name| (name.clone(), Value::String(REDACTED.to_string())))
        .collect()
}

fn redact_named_values(values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(name, value)| {
            let value = if !is_symbolic_reference(value) {
                REDACTED.to_string()
            } else {
                value.clone()
            };
            (name.clone(), value)
        })
        .collect()
}

fn is_sensitive_url_key(name: &str) -> bool {
    let name = percent_decode(name).to_ascii_lowercase();
    is_sensitive_env_name(&name)
        || name.contains("auth")
        || name.contains("signature")
        || name == "sig"
        || name == "key"
}

fn percent_decode(value: &str) -> String {
    let mut decoded = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let pair = bytes.next().zip(bytes.next());
            if let Some((high, low)) = pair.and_then(|(high, low)| {
                Some(((high as char).to_digit(16)?, (low as char).to_digit(16)?))
            }) {
                decoded.push((high * 16 + low) as u8);
                continue;
            }
        }
        decoded.push(byte);
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

pub fn literal_credential_in_url(url: &str) -> bool {
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    if let Some((userinfo, _)) = authority.rsplit_once('@') {
        if !userinfo.split(':').all(is_symbolic_reference) {
            return true;
        }
    }
    if let Some((_, rest)) = url.split_once("://") {
        let path = rest
            .split_once('/')
            .map(|(_, path)| path.split(['?', '#']).next().unwrap_or(""))
            .unwrap_or("");
        let mut segments = path.split('/');
        while let Some(segment) = segments.next() {
            if is_sensitive_url_key(segment)
                && segments
                    .next()
                    .is_some_and(|value| !is_symbolic_reference(value))
            {
                return true;
            }
        }
    }
    if let Some((_, query)) = url.split_once('?') {
        let query = query.split('#').next().unwrap_or("");
        for part in query.split('&') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            if is_sensitive_url_key(key) && !is_symbolic_reference(value) {
                return true;
            }
        }
    }
    if let Some((_, fragment)) = url.split_once('#') {
        if !fragment.is_empty() && !is_symbolic_reference(fragment) {
            return true;
        }
    }
    false
}

pub(crate) fn redact_url(url: &str) -> String {
    if literal_credential_in_url(url) {
        return REDACTED.to_string();
    }
    // Query values can be secrets even when their keys are unfamiliar. Keep only
    // symbolic references in diagnostic output.
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let query = query
        .split('&')
        .map(|part| {
            let Some((key, value)) = part.split_once('=') else {
                return if is_symbolic_reference(part) {
                    part.to_string()
                } else {
                    REDACTED.to_string()
                };
            };
            format!(
                "{key}={}",
                if is_symbolic_reference(value) {
                    value
                } else {
                    REDACTED
                }
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{query}")
}

pub fn plugin_storage_key(id: &str) -> String {
    let digest = Sha256::digest(id.as_bytes());
    format!("v1-{digest:x}")
}

pub fn is_sensitive_env_name(name: &str) -> bool {
    let normalized = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "APIKEY",
        "PRIVATE_KEY",
        "ACCESS_KEY",
        "CREDENTIAL",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

pub fn is_sensitive_header_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "api-key"
            | "x-auth-token"
            | "cookie"
            | "set-cookie"
    )
}

pub fn is_symbolic_reference(value: &str) -> bool {
    let expression = value.strip_prefix("Bearer ").unwrap_or(value);
    let Some(expression) = expression
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
    else {
        return false;
    };
    if expression.contains(['$', '{', '}']) {
        return false;
    }
    let expression = expression
        .strip_prefix("env:")
        .or_else(|| expression.strip_prefix("input:"))
        .unwrap_or(expression);
    let name = match expression.split_once(":-") {
        Some((name, "")) => name,
        Some(_) => return false,
        None => expression,
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostic_serialization_and_debug_hide_retained_secrets() {
        let server: McpServerDefinition = serde_json::from_value(json!({
            "name": "private", "transport": "http",
            "url": "https://example.test/mcp?custom=top-secret",
            "command": "launch https://user:top-secret@example.test/mcp",
            "args": ["--endpoint=https://example.test/mcp?sig=top-secret"],
            "cwd": "https://example.test/mcp?sig=top-secret",
            "env": {"CUSTOM": "top-secret", "TOKEN": "${env:TOKEN}"},
            "headers": {"X-Custom": "top-secret", "Authorization": "Bearer ${TOKEN}"},
            "extensions": {"vendor": {"token": "top-secret"}}
        }))
        .unwrap();
        let input: InputDefinition = serde_json::from_value(json!({
            "name": "credential", "type": "password", "required": false,
            "sensitive": true, "default": "top-secret",
            "constraints": {"vendor": "top-secret"}
        }))
        .unwrap();
        let public_input: InputDefinition = serde_json::from_value(json!({
            "name": "endpoint", "type": "string", "required": false,
            "sensitive": false,
            "default": {"nested": [{"url": "https://example.test/mcp?sig=top-secret"}]}
        }))
        .unwrap();
        let catalog: CatalogEntry = serde_json::from_value(json!({
            "name": "private", "source": {"kind": "git", "url": "https://user:top-secret@github.com/example/repo.git"},
            "format": "claude", "raw": {"token": "top-secret"}
        }))
        .unwrap();
        let marketplace: MarketplaceRecord = serde_json::from_value(json!({
            "name": "private", "source": {"kind": "local", "path": "https://user:top-secret@example.test/catalog"},
            "format": "claude", "catalog_path": "marketplace.json", "entries": []
        })).unwrap();
        for output in [
            serde_json::to_string(&server).unwrap(),
            format!("{server:?}"),
            serde_json::to_string(&input).unwrap(),
            format!("{input:?}"),
            serde_json::to_string(&public_input).unwrap(),
            format!("{public_input:?}"),
            serde_json::to_string(&catalog).unwrap(),
            format!("{catalog:?}"),
            serde_json::to_string(&marketplace).unwrap(),
            format!("{marketplace:?}"),
        ] {
            assert!(!output.contains("top-secret"), "{output}");
        }
        let json = serde_json::to_value(&server).unwrap();
        assert_eq!(json["env"]["TOKEN"], "${env:TOKEN}");
        assert_eq!(json["headers"]["Authorization"], "Bearer ${TOKEN}");
        assert_eq!(json["url"], "https://example.test/mcp?custom=[redacted]");
        assert_eq!(json["args"][0], "--endpoint=[redacted]");
        assert_eq!(json["extensions"]["vendor"], "[redacted]");
        assert!(json.get("cwd").is_some());
        assert_eq!(json["transport"], "http");
        let catalog_json = serde_json::to_value(&catalog).unwrap();
        assert_eq!(catalog_json["source"]["kind"], "git");
        assert!(catalog_json.get("description").is_none());
        assert_eq!(catalog_json["raw"], "[redacted]");
        let marketplace_json = serde_json::to_value(&marketplace).unwrap();
        assert_eq!(marketplace_json["source"]["kind"], "local");
        assert!(marketplace_json.get("resolved_revision").is_none());
        assert_eq!(
            serde_json::to_value(&input).unwrap()["default"],
            "[redacted]"
        );
        assert_eq!(
            serde_json::to_value(&public_input).unwrap()["default"]["nested"][0]["url"],
            "[redacted]"
        );
    }
}

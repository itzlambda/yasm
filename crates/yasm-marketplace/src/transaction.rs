//! Recoverable changes to owned output entries. Unrelated configuration is never journaled.
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item, Table};

use crate::model::{plugin_storage_key, InstallationRecord};
use crate::state::{
    load_installations, save_installations, save_json, write_atomic, InstallationState,
    MarketplacePaths, RawInstallationRecord,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum OutputKind {
    SkillLink,
    JsonMcp,
    TomlMcp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub kind: OutputKind,
    pub path: Utf8PathBuf,
    pub name: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

impl Change {
    fn matches(&self, current: &Option<Value>, expected: &Option<Value>) -> bool {
        if self.kind == OutputKind::SkillLink {
            if let (Some(Value::String(current)), Some(Value::String(expected))) =
                (current, expected)
            {
                return Utf8Path::new(current) == Utf8Path::new(expected);
            }
        }
        current == expected
    }

    pub fn validate(&self) -> Result<()> {
        if !self.matches(&self.current()?, &self.before) {
            if self.before.is_some() && self.kind != OutputKind::SkillLink {
                bail!(
                    "refusing to overwrite modified MCP entry `{}` in {}",
                    self.name,
                    self.path
                );
            }
            let alias = if self.kind == OutputKind::SkillLink {
                "--skill-alias"
            } else {
                "--mcp-alias"
            };
            bail!(
                "output `{}` already exists or was modified in {}; refusing to overwrite it (for a new output, use {alias} SOURCE=OUTPUT)",
                self.name,
                self.path
            );
        }
        Ok(())
    }

    pub fn current(&self) -> Result<Option<Value>> {
        match self.kind {
            OutputKind::SkillLink => match std::fs::symlink_metadata(&self.path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let target = Utf8PathBuf::from_path_buf(std::fs::read_link(&self.path)?)
                        .map_err(|_| anyhow::anyhow!("non-UTF-8 skill link"))?;
                    Ok(Some(Value::String(target.to_string())))
                }
                Ok(_) => bail!("refusing to change non-link skill output {}", self.path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            },
            OutputKind::JsonMcp => {
                let root = read_json(&self.path)?;
                let map = root.get("mcpServers");
                if map.is_some_and(|map| !map.is_object()) {
                    bail!("mcpServers must be an object");
                }
                Ok(map.and_then(|map| map.get(&self.name)).cloned())
            }
            OutputKind::TomlMcp => {
                let document = read_toml(&self.path)?;
                if document
                    .get("mcp_servers")
                    .is_some_and(|item| !item.is_table())
                {
                    bail!("mcp_servers must be a table");
                }
                let parsed: Value = toml_edit::de::from_str(&document.to_string())?;
                Ok(parsed
                    .get("mcp_servers")
                    .and_then(|map| map.get(&self.name))
                    .cloned())
            }
        }
    }

    fn write(&self, desired: &Option<Value>) -> Result<()> {
        match self.kind {
            OutputKind::SkillLink => {
                if let Some(value) = desired {
                    let target = value
                        .as_str()
                        .context("invalid skill link in transaction")?;
                    let parent = self.path.parent().context("skill link has no parent")?;
                    std::fs::create_dir_all(parent)?;
                    let staging = tempfile::tempdir_in(parent)?;
                    let staged = Utf8Path::from_path(staging.path())
                        .context("non-UTF-8 staging path")?
                        .join("link");
                    yasm_core::ensure_skill_symlink(&staged, Utf8Path::new(target))?;
                    // Publish the replacement atomically: a failed replacement leaves the old link intact.
                    std::fs::rename(staged, &self.path)?;
                } else {
                    std::fs::remove_file(&self.path)?;
                }
                Ok(())
            }
            OutputKind::JsonMcp => {
                let mut root = read_json(&self.path)?;
                let map = root
                    .as_object_mut()
                    .context("MCP configuration must be an object")?
                    .entry("mcpServers")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .context("mcpServers must be an object")?;
                match desired {
                    Some(value) => {
                        map.insert(self.name.clone(), value.clone());
                    }
                    None => {
                        map.remove(&self.name);
                    }
                }
                save_json(&self.path, &root)
            }
            OutputKind::TomlMcp => {
                let mut document = read_toml(&self.path)?;
                if document.get("mcp_servers").is_none() {
                    document["mcp_servers"] = Item::Table(Table::new());
                }
                let map = document["mcp_servers"]
                    .as_table_mut()
                    .context("mcp_servers must be a table")?;
                match desired {
                    Some(value) => {
                        let encoded = toml_edit::ser::to_document(&json!({"entry": value}))?;
                        let table = encoded["entry"]
                            .clone()
                            .into_table()
                            .map_err(|_| anyhow::anyhow!("MCP entry must be a table"))?;
                        map.insert(&self.name, Item::Table(table));
                    }
                    None => {
                        map.remove(&self.name);
                    }
                }
                write_atomic(&self.path, document.to_string().as_bytes())
            }
        }
    }
}

/// Combine disable/enable plans into one replacement of each output.
pub fn compose(changes: Vec<Change>) -> Result<Vec<Change>> {
    let mut combined: Vec<Change> = Vec::new();
    for change in changes {
        if let Some(previous) = combined.iter_mut().find(|old| {
            old.kind == change.kind && old.path == change.path && old.name == change.name
        }) {
            if previous.after != change.before {
                bail!("conflicting output plan for {}", change.name);
            }
            previous.after = change.after;
        } else {
            combined.push(change);
        }
    }
    Ok(combined)
}

#[derive(Deserialize)]
struct Pending {
    id: String,
    generation: u64,
    before: Option<InstallationRecord>,
    after: Option<InstallationRecord>,
    changes: Vec<Change>,
    cleanup: Vec<Utf8PathBuf>,
}

impl Serialize for Pending {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct RawPending<'a> {
            id: &'a str,
            generation: u64,
            before: Option<RawInstallationRecord<'a>>,
            after: Option<RawInstallationRecord<'a>>,
            changes: &'a Vec<Change>,
            cleanup: &'a Vec<Utf8PathBuf>,
        }
        RawPending {
            id: &self.id,
            generation: self.generation,
            before: self.before.as_ref().map(RawInstallationRecord::from),
            after: self.after.as_ref().map(RawInstallationRecord::from),
            changes: &self.changes,
            cleanup: &self.cleanup,
        }
        .serialize(serializer)
    }
}

pub fn commit(
    paths: &MarketplacePaths,
    state: &mut InstallationState,
    id: &str,
    after: Option<InstallationRecord>,
    changes: Vec<Change>,
    cleanup: Vec<Utf8PathBuf>,
) -> Result<()> {
    let mut changes = compose(changes)?;
    for change in &changes {
        change.validate()?;
    }
    changes.retain(|change| change.before != change.after);
    let pending = Pending {
        id: id.to_string(),
        generation: state
            .generation
            .checked_add(1)
            .context("installation generation overflow")?,
        before: state.plugins.get(id).cloned(),
        after,
        changes,
        cleanup,
    };
    let journal = paths
        .operations()
        .join(format!("pending-{}.json", plugin_storage_key(id)));
    save_json(&journal, &pending)?;
    let result = (|| -> Result<()> {
        for change in &pending.changes {
            change.validate()?;
            change.write(&change.after)?;
        }
        match &pending.after {
            Some(record) => {
                state.plugins.insert(id.to_string(), record.clone());
            }
            None => {
                state.plugins.remove(id);
            }
        }
        state.generation = pending.generation;
        save_installations(paths, state)?;
        Ok(())
    })();
    if let Err(error) = result {
        // The durable state decides whether publication completed, even if the caller errored.
        if let Err(recovery) = recover_one(paths, &journal, &pending) {
            return Err(error).context(format!(
                "rollback requires recovery: {recovery}; retry after resolving the output conflict"
            ));
        }
        return Err(error);
    }
    finish_cleanup(&journal, &pending).context(
        "plugin changes committed; cleanup remains pending and will retry on the next mutation",
    )
}

pub fn recover(paths: &MarketplacePaths) -> Result<()> {
    if !paths.operations().exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(paths.operations())? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("pending-") {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|_| anyhow::anyhow!("non-UTF-8 journal path"))?;
        let pending: Pending = serde_json::from_slice(&std::fs::read(&path)?)?;
        recover_one(paths, &path, &pending)?;
    }
    Ok(())
}

fn recover_one(paths: &MarketplacePaths, journal: &Utf8Path, pending: &Pending) -> Result<()> {
    let state = load_installations(paths)?;
    let current = raw_record_value(state.plugins.get(&pending.id))?;
    if state.generation == pending.generation
        && current == raw_record_value(pending.after.as_ref())?
    {
        return finish_cleanup(journal, pending);
    }
    if state.generation.checked_add(1) != Some(pending.generation)
        || current != raw_record_value(pending.before.as_ref())?
    {
        bail!(
            "installation state changed during recovery for {}",
            pending.id
        );
    }
    for change in pending.changes.iter().rev() {
        let current = change.current()?;
        if change.matches(&current, &change.before) {
            continue;
        }
        if !change.matches(&current, &change.after) {
            bail!(
                "output `{}` was modified in {}; recovery will not overwrite it",
                change.name,
                change.path
            );
        }
        change.write(&change.before)?;
    }
    std::fs::remove_file(journal)?;
    Ok(())
}

fn raw_record_value(record: Option<&InstallationRecord>) -> Result<Value> {
    record
        .map(|record| serde_json::to_value(RawInstallationRecord::from(record)))
        .transpose()
        .map(|value| value.unwrap_or(Value::Null))
        .map_err(Into::into)
}

fn finish_cleanup(journal: &Utf8Path, pending: &Pending) -> Result<()> {
    for path in &pending.cleanup {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                std::fs::remove_dir_all(path)?
            }
            Ok(_) => bail!("refusing to remove unmanaged plugin path {path}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    std::fs::remove_file(journal)?;
    Ok(())
}

fn read_json(path: &Utf8Path) -> Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("failed to parse MCP configuration"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.into()),
    }
}
fn read_toml(path: &Utf8Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) => text.parse().context("failed to parse Codex configuration"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(error) => Err(error.into()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, MarketplacePaths, InstallationRecord) {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let paths = MarketplacePaths::new(&root.join("data"), &root.join("cache"));
        let record = serde_json::from_value(json!({
            "id": "demo@test", "marketplace": "test", "entry_name": "demo",
            "marketplace_source": {"kind": "local", "path": "source"},
            "catalog_format": "claude", "source": {"kind": "relative", "path": "demo"},
            "catalog_entry": {
                "name": "demo", "source": {"kind": "relative", "path": "demo"},
                "format": "claude", "raw": null
            },
            "snapshot": root.join("snapshot"), "enabled": [], "outputs": [],
            "definition": {"name": "demo", "skills": [], "mcpServers": [], "inputs": [], "import": {
                "format": "claude", "marketplace_entry": "demo", "digest": "fixture"
            }}
        }))
        .unwrap();
        (temp, paths, record)
    }

    #[test]
    fn journal_and_state_preserve_operational_values_while_model_output_redacts() {
        let (_temp, paths, mut record) = fixture();
        let source = crate::model::PluginSource::Git {
            url: "https://user:top-secret@github.com/example/repo.git".to_string(),
            path: None,
            r#ref: None,
            sha: None,
        };
        record.source = source.clone();
        record.catalog_entry.source = source.clone();
        record.marketplace_source.path = "https://user:top-secret@example.test/catalog".to_string();
        record.definition.mcp_servers.push(
            serde_json::from_value(json!({
                "name": "private", "transport": "http",
                "url": "https://example.test/mcp?token=top-secret",
                "headers": {"Authorization": "${TOKEN}"},
                "extensions": {"vendor": "top-secret"}
            }))
            .unwrap(),
        );
        assert!(!serde_json::to_string(&record)
            .unwrap()
            .contains("top-secret"));
        assert!(!format!("{record:?}").contains("top-secret"));
        let pending = Pending {
            id: record.id.clone(),
            generation: 1,
            before: None,
            after: Some(record.clone()),
            changes: Vec::new(),
            cleanup: Vec::new(),
        };
        let journal = serde_json::to_vec(&pending).unwrap();
        assert!(String::from_utf8_lossy(&journal).contains("top-secret"));
        let restored: Pending = serde_json::from_slice(&journal).unwrap();
        assert_eq!(
            restored.after.unwrap().definition.mcp_servers[0].url,
            record.definition.mcp_servers[0].url
        );
        let mut state = InstallationState::default();
        state.plugins.insert(record.id.clone(), record);
        save_installations(&paths, &state).unwrap();
        let stored = std::fs::read_to_string(paths.installations()).unwrap();
        assert!(stored.contains("top-secret"));
        let loaded = load_installations(&paths).unwrap();
        assert_eq!(
            loaded.plugins["demo@test"].definition.mcp_servers[0].extensions["vendor"],
            "top-secret"
        );
        assert_eq!(loaded.plugins["demo@test"].source, source);
    }

    fn mcp_change(paths: &MarketplacePaths) -> Change {
        Change {
            kind: OutputKind::JsonMcp,
            path: paths.root.join("config"),
            name: "owned".into(),
            before: None,
            after: Some(json!({"command": "server"})),
        }
    }

    fn journal(paths: &MarketplacePaths, pending: &Pending) -> Utf8PathBuf {
        let path = paths.operations().join("pending-test.json");
        save_json(&path, pending).unwrap();
        path
    }

    #[test]
    fn interrupted_output_write_is_reversed_without_losing_user_entries() {
        for kind in [OutputKind::JsonMcp, OutputKind::TomlMcp] {
            let (_temp, paths, record) = fixture();
            let change = Change {
                kind,
                ..mcp_change(&paths)
            };
            let pending = Pending {
                generation: 1,
                id: record.id.clone(),
                before: None,
                after: Some(record),
                changes: vec![change.clone()],
                cleanup: Vec::new(),
            };
            let journal = journal(&paths, &pending);
            change.write(&change.after).unwrap();
            // Simulate an unrelated edit after interruption, before recovery.
            let user = Change {
                name: "user".into(),
                after: Some(json!({"command": "user-server"})),
                ..change.clone()
            };
            user.write(&user.after).unwrap();
            recover(&paths).unwrap();
            assert_eq!(change.current().unwrap(), None);
            assert_eq!(user.current().unwrap(), user.after);
            assert!(!journal.exists());
            assert!(load_installations(&paths).unwrap().plugins.is_empty());
        }
    }

    #[test]
    fn recovery_uses_commit_marker_when_installation_record_is_unchanged() {
        let (_temp, paths, record) = fixture();
        let mut state = InstallationState::default();
        state.plugins.insert(record.id.clone(), record.clone());
        save_installations(&paths, &state).unwrap();
        let change = mcp_change(&paths);
        let pending = Pending {
            id: record.id.clone(),
            generation: 1,
            before: Some(record.clone()),
            after: Some(record),
            changes: vec![change.clone()],
            cleanup: Vec::new(),
        };
        journal(&paths, &pending);
        change.write(&change.after).unwrap();
        recover(&paths).unwrap();
        assert_eq!(change.current().unwrap(), None);
        assert_eq!(load_installations(&paths).unwrap().generation, 0);
    }

    #[test]
    fn interrupted_committed_operation_keeps_outputs() {
        let (_temp, paths, record) = fixture();
        let change = mcp_change(&paths);
        let pending = Pending {
            generation: 1,
            id: record.id.clone(),
            before: None,
            after: Some(record.clone()),
            changes: vec![change.clone()],
            cleanup: Vec::new(),
        };
        let journal = journal(&paths, &pending);
        change.write(&change.after).unwrap();
        let mut state = InstallationState::default();
        state.plugins.insert(record.id.clone(), record);
        state.generation = 1;
        save_installations(&paths, &state).unwrap();
        recover(&paths).unwrap();
        assert_eq!(change.current().unwrap(), change.after);
        assert!(!journal.exists());
    }

    #[test]
    fn recovery_keeps_journal_and_refuses_modified_owned_entries() {
        let (_temp, paths, record) = fixture();
        let change = mcp_change(&paths);
        let pending = Pending {
            generation: 1,
            id: record.id.clone(),
            before: None,
            after: Some(record),
            changes: vec![change.clone()],
            cleanup: Vec::new(),
        };
        let journal = journal(&paths, &pending);
        let edited = Some(json!({"command": "edited"}));
        change.write(&edited).unwrap();
        assert!(recover(&paths)
            .unwrap_err()
            .to_string()
            .contains("will not overwrite"));
        assert_eq!(change.current().unwrap(), edited);
        assert!(journal.exists());
        change.write(&change.after).unwrap();
        recover(&paths).unwrap();
        assert_eq!(change.current().unwrap(), None);
    }

    #[test]
    fn state_publication_failure_rolls_back_outputs() {
        let (_temp, paths, record) = fixture();
        // Block state persistence after output writes have completed. Recovery must
        // retain its journal until the state path becomes readable again.
        let change = mcp_change(&paths);
        std::fs::create_dir_all(paths.installations()).unwrap();
        let mut state = InstallationState::default();
        let id = record.id.clone();
        assert!(commit(
            &paths,
            &mut state,
            &id,
            Some(record),
            vec![change.clone()],
            Vec::new()
        )
        .is_err());
        // Recovery is blocked until the external state obstruction is removed.
        std::fs::remove_dir(paths.installations()).unwrap();
        recover(&paths).unwrap();
        assert_eq!(change.current().unwrap(), None);
        assert!(load_installations(&paths).unwrap().plugins.is_empty());
    }

    #[test]
    fn committed_removal_recovers_pending_data_cleanup() {
        let (_temp, paths, record) = fixture();
        let mut state = InstallationState::default();
        state.plugins.insert(record.id.clone(), record.clone());
        save_installations(&paths, &state).unwrap();
        let data = paths.data().join(record.storage_key());
        std::fs::create_dir_all(paths.data()).unwrap();
        std::fs::write(&data, "obstruction").unwrap();
        let error = commit(
            &paths,
            &mut state,
            &record.id,
            None,
            Vec::new(),
            vec![data.clone()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("cleanup remains pending"));
        assert!(load_installations(&paths).unwrap().plugins.is_empty());
        assert_eq!(std::fs::read_dir(paths.operations()).unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string(&data).unwrap(), "obstruction");
        std::fs::remove_file(&data).unwrap();
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("runtime-data"), "temporary").unwrap();
        recover(&paths).unwrap();
        assert!(!data.exists());
        assert_eq!(std::fs::read_dir(paths.operations()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_update_restores_previous_link_and_mcp() {
        let (_temp, paths, record) = fixture();
        let mut updated = record.clone();
        updated.definition.version = Some("2".into());
        let mut state = InstallationState::default();
        state.plugins.insert(record.id.clone(), record.clone());
        save_installations(&paths, &state).unwrap();
        let changes = vec![
            Change {
                before: Some(json!({"command":"old"})),
                after: Some(json!({"command":"new"})),
                ..mcp_change(&paths)
            },
            Change {
                kind: OutputKind::SkillLink,
                path: paths.root.join("skills/demo"),
                name: "demo".into(),
                before: Some(json!("../old")),
                after: Some(json!("../new")),
            },
        ];
        for change in &changes {
            change.write(&change.before).unwrap();
        }
        let pending = Pending {
            generation: 1,
            id: record.id.clone(),
            before: Some(record.clone()),
            after: Some(updated),
            changes: changes.clone(),
            cleanup: Vec::new(),
        };
        journal(&paths, &pending);
        for change in &changes {
            change.write(&change.after).unwrap();
        }
        recover(&paths).unwrap();
        for change in &changes {
            assert_eq!(change.current().unwrap(), change.before);
        }
    }
}

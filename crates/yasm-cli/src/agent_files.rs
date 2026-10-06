//! Adoption of complete instruction files, independent from skill lock records.
use std::io::Write;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{MigrationAction, ResolvedScope, ScopeContext};

const JOURNAL: &str = "agent-file-migration.json";
const NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

#[derive(Debug)]
enum Disposition {
    Missing,
    Managed,
    Adopt {
        contents: Vec<u8>,
        store_created: bool,
    },
    Conflict(String),
}

#[derive(Debug)]
struct Candidate {
    source: Utf8PathBuf,
    destination: Utf8PathBuf,
    disposition: Disposition,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    committed: bool,
    changes: Vec<Change>,
}

#[derive(Serialize, Deserialize)]
struct Change {
    source: Utf8PathBuf,
    resolved_parent: Utf8PathBuf,
    destination: Utf8PathBuf,
    target: Utf8PathBuf,
    digest: String,
    store_created: bool,
}

fn journal_path(context: &ScopeContext) -> Utf8PathBuf {
    context.paths.data_dir.join(JOURNAL)
}

pub(crate) fn require_no_pending(context: &ScopeContext) -> Result<()> {
    if present(&journal_path(context))? {
        anyhow::bail!("an interrupted agent-file migration must be recovered first; run `yasm migrate --agent-files --action apply` with the original scope and CODEX_HOME");
    }
    Ok(())
}

// Lock the store directory itself: canonical aliases share one lock and no
// runtime lock file needs to be committed with a project store.
struct MigrationLease(std::fs::File);

impl MigrationLease {
    fn acquire(context: &ScopeContext) -> Result<Self> {
        std::fs::create_dir_all(&context.paths.data_dir)?;
        let file = std::fs::File::open(&context.paths.data_dir)?;
        file.try_lock().map_err(|error| anyhow::anyhow!(
            "cannot lock agent-file migration scope; another migration may be active; retry after it finishes: {error}"
        ))?;
        Ok(Self(file))
    }
}

impl Drop for MigrationLease {
    fn drop(&mut self) {
        // Release explicitly even if a concurrent fork inherited this descriptor.
        let _ = self.0.unlock();
    }
}

fn sources(context: &ScopeContext) -> Result<[Utf8PathBuf; 2]> {
    match &context.scope {
        ResolvedScope::Project { root } => Ok(NAMES.map(|name| root.join(name))),
        ResolvedScope::Global => {
            let home = Utf8PathBuf::from_path_buf(etcetera::home_dir()?)
                .map_err(|_| anyhow::anyhow!("home directory must be a UTF-8 path"))?;
            let codex = match std::env::var_os("CODEX_HOME") {
                Some(value) => {
                    let path = Utf8PathBuf::from_path_buf(value.into())
                        .map_err(|_| anyhow::anyhow!("CODEX_HOME must be a UTF-8 path"))?;
                    anyhow::ensure!(!path.as_str().is_empty(), "CODEX_HOME is set but empty");
                    if path.is_absolute() {
                        path
                    } else {
                        crate::current_dir_utf8()?.join(path)
                    }
                }
                None => home.join(".codex"),
            };
            Ok([codex.join("AGENTS.md"), home.join(".claude/CLAUDE.md")])
        }
    }
}

pub(crate) fn migrate(context: &ScopeContext, requested: Option<MigrationAction>) -> Result<()> {
    let mut requested = requested;
    // Acquire before reading the journal so an active migration cannot be
    // mistaken for an interrupted one. Review never creates or locks files.
    let mut lease = if requested == Some(MigrationAction::Apply) {
        Some(MigrationLease::acquire(context)?)
    } else {
        None
    };
    // Skill recovery must remain a separate operation, especially for read-only review.
    if present(&crate::migration_journal_path(context))? {
        anyhow::bail!("an interrupted skill migration must be recovered first; run `yasm migrate --action review`");
    }
    let source_paths = sources(context)?;
    if context.is_global() && present(&source_paths[0].with_file_name("AGENTS.override.md"))? {
        eprintln!(
            "warning: {} takes precedence over AGENTS.md; the override will not be migrated",
            display(&source_paths[0].with_file_name("AGENTS.override.md"))
        );
    }
    if present(&journal_path(context))? {
        if requested == Some(MigrationAction::Review) {
            anyhow::bail!("an interrupted agent-file migration is pending; review made no changes; run `yasm migrate --agent-files --action apply` to recover it");
        }
        let action = choose_action(requested)?;
        if action == MigrationAction::Review {
            println!("review only; pending migration was not changed");
            return Ok(());
        }
        if lease.is_none() {
            lease = Some(MigrationLease::acquire(context)?);
        }
        if present(&journal_path(context))? {
            recover(context, &source_paths)?;
        }
        requested = Some(MigrationAction::Apply);
    }
    let store = context.paths.data_dir.join("agent-files");
    if present(&store)? {
        anyhow::ensure!(
            std::fs::symlink_metadata(&store)?.is_dir(),
            "agent-files store must be a regular directory, not a symlink"
        );
    }
    let candidates = source_paths
        .iter()
        .zip(NAMES)
        .map(|(source, name)| inspect(source, &store.join(name)))
        .collect::<Result<Vec<_>>>()?;
    crate::print_migration_scope(context);
    for candidate in &candidates {
        let state = match &candidate.disposition {
            Disposition::Missing => "missing; skipped".to_string(),
            Disposition::Managed => "already managed".to_string(),
            Disposition::Adopt {
                store_created: true,
                ..
            } => "migrate new".to_string(),
            Disposition::Adopt { .. } => "adopt identical stored file".to_string(),
            Disposition::Conflict(reason) => format!("CONFLICT: {reason}"),
        };
        println!(
            "  {} -> {} (relative symlink at original location): {state}",
            display(&candidate.source),
            display(&candidate.destination)
        );
    }
    if choose_action(requested)? == MigrationAction::Review {
        println!("review only; no changes made");
        return Ok(());
    }
    if lease.is_none() {
        lease = Some(MigrationLease::acquire(context)?);
        // Another apply may have run while the interactive choice was pending.
        // Revalidate the plan below, and defer a newly pending journal to retry.
        require_no_pending(context)?;
    }
    let _lease = lease;
    for candidate in &candidates {
        if let Disposition::Conflict(reason) = &candidate.disposition {
            anyhow::bail!(
                "cannot migrate {}: {reason}; resolve the conflict and run `--action review` again",
                display(&candidate.source)
            );
        }
    }
    let selected = candidates
        .iter()
        .filter(|c| matches!(c.disposition, Disposition::Adopt { .. }))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        println!("no unmanaged agent files found");
        return Ok(());
    }
    // Recheck the complete plan before making any changes.
    for candidate in &selected {
        let current = inspect(&candidate.source, &candidate.destination)?;
        let Disposition::Adopt {
            contents,
            store_created,
        } = &candidate.disposition
        else {
            continue;
        };
        anyhow::ensure!(
            matches!(current.disposition, Disposition::Adopt { contents: ref now, store_created: created } if now == contents && created == *store_created),
            "agent-file state changed after review; run `--action review` again"
        );
    }
    std::fs::create_dir_all(&store)?;
    let resolved_store = crate::canonical_utf8(&store)?;
    let mut journal = Journal {
        version: 1,
        committed: false,
        changes: Vec::new(),
    };
    for candidate in &selected {
        let parent = candidate
            .source
            .parent()
            .context("agent file has no parent")?;
        let resolved_parent = crate::canonical_utf8(parent)?;
        let name = candidate
            .source
            .file_name()
            .context("agent file has no name")?;
        let destination = resolved_store.join(name);
        let Disposition::Adopt {
            contents,
            store_created,
        } = &candidate.disposition
        else {
            continue;
        };
        let change = Change {
            source: candidate.source.clone(),
            target: relative_path(&resolved_parent, &destination)?,
            resolved_parent,
            destination,
            digest: digest(contents),
            store_created: *store_created,
        };
        anyhow::ensure!(
            !present(&backup(&change))?,
            "migration backup already exists at {}; preserve it before retrying",
            display(&backup(&change))
        );
        journal.changes.push(change);
    }
    write_journal(context, &journal)?;
    let result = (|| -> Result<()> {
        for (candidate, change) in selected.iter().zip(&journal.changes) {
            let Disposition::Adopt { contents, .. } = &candidate.disposition else {
                continue;
            };
            anyhow::ensure!(
                crate::canonical_utf8(candidate.source.parent().context("missing parent")?)?
                    == change.resolved_parent,
                "agent-file parent changed after review"
            );
            anyhow::ensure!(
                regular_contents(&change.source)? == *contents,
                "agent file changed after review"
            );
            if change.store_created {
                let mut staged = tempfile::NamedTempFile::new_in(&resolved_store)?;
                staged.write_all(contents)?;
                staged
                    .as_file()
                    .set_permissions(std::fs::metadata(&change.source)?.permissions())?;
                staged.as_file().sync_all()?;
                staged
                    .persist_noclobber(&change.destination)
                    .map_err(|error| error.error)?;
                crate::sync_directory(&resolved_store)?;
            } else {
                anyhow::ensure!(
                    regular_contents(&change.destination)? == *contents,
                    "stored file changed after review"
                );
            }
            crate::rename_directory_noreplace(&change.source, &backup(change))?;
            crate::sync_directory(&change.resolved_parent)?;
            file_symlink(&change.target, &change.source)?;
            crate::sync_directory(&change.resolved_parent)?;
        }
        journal.committed = true;
        write_journal(context, &journal)?;
        Ok(())
    })();
    if let Err(error) = result {
        recover(context, &source_paths).context("agent-file rollback could not complete; keep the journal and backups and retry with --action apply")?;
        return Err(error);
    }
    recover(context, &source_paths)?;
    println!("migrated {} agent file(s)", selected.len());
    if context.is_project() {
        println!("Commit .yasm/agent-files/ and the root AGENTS.md / CLAUDE.md links so a clone can use them without Yasm.");
    }
    Ok(())
}

fn choose_action(requested: Option<MigrationAction>) -> Result<MigrationAction> {
    if let Some(action) = requested {
        return Ok(action);
    }
    let selection = crate::interactive::ask_select(
        "migration action",
        "Choose an agent-file migration action",
        &["Review".to_string(), "Apply".to_string()],
        "pass `--action apply` or `--action review`",
    )?;
    Ok(if selection == 0 {
        MigrationAction::Review
    } else {
        MigrationAction::Apply
    })
}

fn inspect(source: &Utf8Path, destination: &Utf8Path) -> Result<Candidate> {
    let disposition = if !present(source)? {
        Disposition::Missing
    } else if std::fs::symlink_metadata(source)?.file_type().is_symlink() {
        let link = std::fs::read_link(source)?;
        let resolved = source.parent().context("missing parent")?.join(
            Utf8PathBuf::from_path_buf(link)
                .map_err(|_| anyhow::anyhow!("non-UTF-8 link target"))?,
        );
        if destination.is_file()
            && resolved.exists()
            && crate::canonical_utf8(&resolved)? == crate::canonical_utf8(destination)?
        {
            if std::fs::symlink_metadata(destination)?.is_file() {
                Disposition::Managed
            } else {
                Disposition::Conflict("stored file is a symlink".into())
            }
        } else {
            Disposition::Conflict(
                "external or broken file symlink; restore a regular file before migration".into(),
            )
        }
    } else if !std::fs::symlink_metadata(source)?.is_file() {
        Disposition::Conflict("source is not a regular file".into())
    } else {
        let contents = regular_contents(source)?;
        if !present(destination)? {
            Disposition::Adopt {
                contents,
                store_created: true,
            }
        } else if !std::fs::symlink_metadata(destination)?.is_file() {
            Disposition::Conflict("stored path is not a regular file".into())
        } else if crate::canonical_utf8(source)? == crate::canonical_utf8(destination)? {
            Disposition::Conflict("original path already resolves to the stored file through its parent; preserve the existing directory link".into())
        } else if regular_contents(destination)? != contents {
            Disposition::Conflict("stored contents differ from the original file".into())
        } else {
            Disposition::Adopt {
                contents,
                store_created: false,
            }
        }
    };
    Ok(Candidate {
        source: source.to_owned(),
        destination: destination.to_owned(),
        disposition,
    })
}

fn backup(change: &Change) -> Utf8PathBuf {
    change.source.with_file_name(format!(
        ".yasm-agent-file-backup-{}",
        change.source.file_name().unwrap_or("unknown")
    ))
}

fn write_journal(context: &ScopeContext, journal: &Journal) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(&context.paths.data_dir)?;
    temporary.write_all(&serde_json::to_vec_pretty(journal)?)?;
    temporary.as_file().sync_all()?;
    if journal.committed {
        temporary
            .persist(journal_path(context))
            .map_err(|error| error.error)?;
    } else {
        temporary
            .persist_noclobber(journal_path(context))
            .map_err(|error| error.error)?;
    }
    crate::sync_directory(&context.paths.data_dir)
}

fn recover(context: &ScopeContext, sources: &[Utf8PathBuf; 2]) -> Result<()> {
    let path = journal_path(context);
    let journal: Journal = serde_json::from_slice(&regular_contents(&path)?)?;
    anyhow::ensure!(
        journal.version == 1,
        "unsupported agent-file migration journal version"
    );
    let store = crate::canonical_utf8(&context.paths.data_dir.join("agent-files"))?;
    let mut seen = std::collections::BTreeSet::new();
    // Validate every recorded location and content before touching any files.
    for change in &journal.changes {
        let name = change
            .source
            .file_name()
            .context("invalid journal source")?;
        anyhow::ensure!(sources.contains(&change.source) && seen.insert(change.source.clone()) && change.destination == store.join(name), "agent-file journal does not match this scope; restore the original scope and CODEX_HOME");
        anyhow::ensure!(
            crate::canonical_utf8(change.source.parent().context("invalid journal parent")?)?
                == change.resolved_parent
                && change.target == relative_path(&change.resolved_parent, &change.destination)?,
            "agent-file parent or journal target changed; backups preserved"
        );
        if journal.committed {
            // A committed transaction can retain its backup after interruption.
            // Never discard the last copy if its stored destination disappeared;
            // post-commit edits remain valid and need not match the old digest.
            regular_contents(&change.destination).context(
                "committed stored file is missing or unreadable; original backup preserved",
            )?;
        }
        if present(&backup(change))? {
            anyhow::ensure!(
                digest(&regular_contents(&backup(change))?) == change.digest,
                "agent-file backup changed; recovery stopped"
            );
            if !journal.committed && present(&change.source)? {
                anyhow::ensure!(
                    std::fs::symlink_metadata(&change.source)?
                        .file_type()
                        .is_symlink()
                        && std::fs::read_link(&change.source)? == change.target.as_std_path(),
                    "original path was recreated or its link changed; backup preserved"
                );
            }
        } else if !journal.committed {
            anyhow::ensure!(
                digest(&regular_contents(&change.source)?) == change.digest,
                "original file changed or is missing; recovery stopped"
            );
        }
        if !journal.committed && present(&change.destination)? {
            anyhow::ensure!(
                digest(&regular_contents(&change.destination)?) == change.digest,
                "stored file changed; recovery stopped to preserve edits"
            );
        }
    }
    for change in journal.changes.iter().rev() {
        let saved = backup(change);
        if present(&saved)? {
            if journal.committed {
                std::fs::remove_file(&saved)?;
            } else {
                if present(&change.source)? {
                    std::fs::remove_file(&change.source)?;
                }
                crate::rename_directory_noreplace(&saved, &change.source)?;
            }
            crate::sync_directory(&change.resolved_parent)?;
        }
        if !journal.committed && change.store_created && present(&change.destination)? {
            std::fs::remove_file(&change.destination)?;
            crate::sync_directory(&store)?;
        }
    }
    std::fs::remove_file(&path)?;
    crate::sync_directory(&context.paths.data_dir)?;
    if !journal.committed {
        eprintln!("recovered an interrupted agent-file migration; original files were restored");
    }
    Ok(())
}

fn present(path: &Utf8Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn regular_contents(path: &Utf8Path) -> Result<Vec<u8>> {
    anyhow::ensure!(
        std::fs::symlink_metadata(path)?.is_file(),
        "{} is not a regular file",
        display(path)
    );
    Ok(std::fs::read(path)?)
}

fn digest(contents: &[u8]) -> String {
    format!("{:x}", Sha256::digest(contents))
}

fn display(path: &Utf8Path) -> String {
    crate::display_user_path(path.as_str())
}

fn relative_path(from: &Utf8Path, to: &Utf8Path) -> Result<Utf8PathBuf> {
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    anyhow::ensure!(
        from.first() == to.first(),
        "cannot create relative link across filesystem roots"
    );
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut path = Utf8PathBuf::new();
    for _ in shared..from.len() {
        path.push("..");
    }
    for component in &to[shared..] {
        path.push(component.as_str());
    }
    Ok(path)
}

#[cfg(unix)]
fn file_symlink(target: &Utf8Path, link: &Utf8Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(windows)]
fn file_symlink(target: &Utf8Path, link: &Utf8Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, link)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use crate::agent_files::{
        backup, digest, file_symlink, inspect, journal_path, migrate, recover, relative_path,
        sources, write_journal, Change, Disposition, Journal,
    };
    use crate::{MigrationAction, ResolvedScope, ScopeContext};
    use camino::Utf8PathBuf;

    struct Fixture {
        _temp: tempfile::TempDir,
        context: ScopeContext,
        journal: Journal,
    }

    impl Fixture {
        fn new(store_created: bool) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root =
                Utf8PathBuf::from_path_buf(std::fs::canonicalize(temp.path()).unwrap()).unwrap();
            let context =
                ScopeContext::from_scope(ResolvedScope::Project { root: root.clone() }).unwrap();
            let store = root.join(".yasm/agent-files");
            std::fs::create_dir_all(&store).unwrap();
            let source = root.join("AGENTS.md");
            let destination = store.join("AGENTS.md");
            std::fs::write(&source, "original").unwrap();
            let change = Change {
                target: relative_path(&root, &destination).unwrap(),
                source,
                resolved_parent: root,
                destination,
                digest: digest(b"original"),
                store_created,
            };
            if !store_created {
                std::fs::write(&change.destination, "original").unwrap();
            }
            Self {
                _temp: temp,
                context,
                journal: Journal {
                    version: 1,
                    committed: false,
                    changes: vec![change],
                },
            }
        }

        fn interrupt(&self, stage: usize) {
            write_journal(&self.context, &self.journal).unwrap();
            let change = &self.journal.changes[0];
            if stage >= 1 {
                std::fs::write(&change.destination, "original").unwrap();
            }
            if stage >= 2 {
                std::fs::rename(&change.source, backup(change)).unwrap();
            }
            if stage >= 3 {
                file_symlink(&change.target, &change.source).unwrap();
            }
        }

        fn recover(&self) -> anyhow::Result<()> {
            recover(&self.context, &sources(&self.context).unwrap())
        }
    }

    #[test]
    fn agent_files_recovery_restores_every_uncommitted_boundary_and_preserves_adopted_store() {
        for store_created in [true, false] {
            for stage in 0..=3 {
                let fixture = Fixture::new(store_created);
                fixture.interrupt(stage);
                fixture.recover().unwrap();
                let change = &fixture.journal.changes[0];
                assert!(std::fs::symlink_metadata(&change.source).unwrap().is_file());
                assert_eq!(std::fs::read(&change.source).unwrap(), b"original");
                assert_eq!(change.destination.exists(), !store_created);
                assert!(!backup(change).exists());
                assert!(!journal_path(&fixture.context).exists());
            }
        }
    }

    #[test]
    fn agent_files_review_does_not_recover_pending_journal_but_apply_recovers_and_migrates() {
        let fixture = Fixture::new(true);
        fixture.interrupt(2);
        let path = journal_path(&fixture.context);
        let before = std::fs::read(&path).unwrap();
        assert!(migrate(&fixture.context, Some(MigrationAction::Review)).is_err());
        let change = &fixture.journal.changes[0];
        assert!(!change.source.exists());
        assert!(backup(change).is_file());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        migrate(&fixture.context, Some(MigrationAction::Apply)).unwrap();
        assert_eq!(
            std::fs::read_link(&change.source).unwrap(),
            change.target.as_std_path()
        );
        assert_eq!(std::fs::read(&change.source).unwrap(), b"original");
        assert!(!path.exists());
    }

    #[test]
    fn agent_files_recovery_completes_committed_cleanup_without_reverting_edits() {
        let mut fixture = Fixture::new(true);
        fixture.interrupt(3);
        fixture.journal.committed = true;
        write_journal(&fixture.context, &fixture.journal).unwrap();
        let change = &fixture.journal.changes[0];
        std::fs::write(&change.source, "edited after commit").unwrap();
        fixture.recover().unwrap();
        assert_eq!(
            std::fs::read(&change.source).unwrap(),
            b"edited after commit"
        );
        assert!(std::fs::symlink_metadata(&change.source)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!backup(change).exists());
    }

    #[test]
    fn committed_agent_files_recovery_preserves_last_copy_when_store_disappears() {
        let mut fixture = Fixture::new(true);
        fixture.interrupt(3);
        fixture.journal.committed = true;
        write_journal(&fixture.context, &fixture.journal).unwrap();
        let change = &fixture.journal.changes[0];
        std::fs::remove_file(&change.destination).unwrap();

        assert!(fixture.recover().is_err());
        assert_eq!(std::fs::read(backup(change)).unwrap(), b"original");
        assert!(journal_path(&fixture.context).exists());
        assert!(std::fs::symlink_metadata(&change.source)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn concurrent_agent_files_apply_cannot_recover_an_active_migration() {
        let fixture = Fixture::new(true);
        let lease = crate::agent_files::MigrationLease::acquire(&fixture.context).unwrap();
        fixture.interrupt(1);
        let journal_before = std::fs::read(journal_path(&fixture.context)).unwrap();
        let change = &fixture.journal.changes[0];

        let error = migrate(&fixture.context, Some(MigrationAction::Apply)).unwrap_err();
        assert!(error
            .to_string()
            .contains("another migration may be active"));
        assert_eq!(std::fs::read(&change.source).unwrap(), b"original");
        assert_eq!(std::fs::read(&change.destination).unwrap(), b"original");
        assert_eq!(
            std::fs::read(journal_path(&fixture.context)).unwrap(),
            journal_before
        );
        assert!(!backup(change).exists());

        drop(lease);
        migrate(&fixture.context, Some(MigrationAction::Apply)).unwrap();
        assert_eq!(std::fs::read(&change.source).unwrap(), b"original");
        assert!(!journal_path(&fixture.context).exists());
    }

    #[test]
    fn agent_files_scope_lock_covers_aliases_and_releases_inherited_descriptors() {
        let mut fixture = Fixture::new(true);
        let lease = crate::agent_files::MigrationLease::acquire(&fixture.context).unwrap();
        let inherited = lease.0.try_clone().unwrap();
        let alias = fixture.journal.changes[0]
            .resolved_parent
            .join("store-alias");
        std::os::unix::fs::symlink(&fixture.context.paths.data_dir, &alias).unwrap();
        fixture.context.paths.data_dir = alias;
        assert!(crate::agent_files::MigrationLease::acquire(&fixture.context).is_err());
        drop(lease);
        let next = crate::agent_files::MigrationLease::acquire(&fixture.context).unwrap();
        drop(next);
        drop(inherited);
    }

    #[test]
    fn agent_files_recovery_is_repeatable_after_partially_completed_rollback() {
        let fixture = Fixture::new(true);
        fixture.interrupt(3);
        let change = &fixture.journal.changes[0];
        std::fs::remove_file(&change.source).unwrap();
        std::fs::rename(backup(change), &change.source).unwrap();
        fixture.recover().unwrap();
        assert_eq!(std::fs::read(&change.source).unwrap(), b"original");
        assert!(!change.destination.exists());
    }

    #[test]
    fn agent_files_recovery_preserves_changed_source_target_backup_and_store() {
        for changed in ["source", "target", "backup", "store"] {
            let fixture = Fixture::new(true);
            fixture.interrupt(3);
            let change = &fixture.journal.changes[0];
            match changed {
                "source" => {
                    std::fs::remove_file(&change.source).unwrap();
                    std::fs::write(&change.source, "recreated").unwrap();
                }
                "target" => {
                    std::fs::remove_file(&change.source).unwrap();
                    file_symlink(camino::Utf8Path::new("other.md"), &change.source).unwrap();
                }
                "backup" => {
                    std::fs::write(backup(change), "backup edit").unwrap();
                }
                _ => {
                    std::fs::write(&change.destination, "store edit").unwrap();
                }
            }
            assert!(fixture.recover().is_err(), "{changed}");
            assert!(backup(change).exists());
            assert!(journal_path(&fixture.context).exists());
            assert!(change.destination.exists());
        }
    }

    #[test]
    fn agent_files_recovery_rejects_forged_scope_and_retargeted_parent() {
        let mut fixture = Fixture::new(true);
        fixture.interrupt(3);
        fixture.journal.changes[0].destination = fixture.context.paths.data_dir.join("outside.md");
        std::fs::write(
            journal_path(&fixture.context),
            serde_json::to_vec(&fixture.journal).unwrap(),
        )
        .unwrap();
        assert!(fixture.recover().is_err());
        assert!(backup(&fixture.journal.changes[0]).exists());

        let mut fixture = Fixture::new(true);
        fixture.interrupt(3);
        fixture.journal.changes[0].resolved_parent = fixture.context.paths.data_dir.clone();
        std::fs::write(
            journal_path(&fixture.context),
            serde_json::to_vec(&fixture.journal).unwrap(),
        )
        .unwrap();
        assert!(fixture.recover().is_err());
        assert!(backup(&fixture.journal.changes[0]).exists());
    }

    #[test]
    fn agent_files_recovery_validates_all_changes_before_restoring_any() {
        let mut fixture = Fixture::new(true);
        let root = fixture.journal.changes[0].resolved_parent.clone();
        let source = root.join("CLAUDE.md");
        let destination = fixture.context.paths.data_dir.join("agent-files/CLAUDE.md");
        let change = Change {
            target: relative_path(&root, &destination).unwrap(),
            source,
            resolved_parent: root,
            destination,
            digest: digest(b"second"),
            store_created: true,
        };
        std::fs::write(backup(&change), "second").unwrap();
        std::fs::write(&change.destination, "second").unwrap();
        file_symlink(&change.target, &change.source).unwrap();
        fixture.journal.changes.push(change);
        fixture.interrupt(3);
        std::fs::write(&fixture.journal.changes[0].destination, "edit").unwrap();
        assert!(fixture.recover().is_err());
        assert!(backup(&fixture.journal.changes[1]).exists());
        assert!(
            std::fs::symlink_metadata(&fixture.journal.changes[1].source)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn agent_files_source_resolving_to_store_through_parent_is_not_relinked() {
        let fixture = Fixture::new(false);
        let store = fixture.context.paths.data_dir.join("agent-files");
        let alias = fixture.journal.changes[0].resolved_parent.join("alias");
        std::os::unix::fs::symlink(&store, &alias).unwrap();
        assert!(matches!(
            inspect(&alias.join("AGENTS.md"), &store.join("AGENTS.md"))
                .unwrap()
                .disposition,
            Disposition::Conflict(_)
        ));
    }
}

mod bundled;
mod source;

pub mod git;

use camino::{Utf8Path, Utf8PathBuf};
use yasm_core::{Error, ResolvedSource, Result, SourceKind, SourceSpec};

pub use bundled::{self_bundle_digest, self_bundle_skills, BundledSkill, SELF_BUNDLE_ID};
pub use source::SourceInput;

#[derive(Debug, Clone)]
pub struct FetchedSource {
    pub root: Utf8PathBuf,
    pub source: SourceSpec,
    pub resolved: Option<ResolvedSource>,
}

#[derive(Debug, Clone)]
pub struct FetchedCheckout {
    pub root: Utf8PathBuf,
    pub resolved: Option<ResolvedSource>,
}

pub fn fetch_source(source: &SourceSpec, destination: &Utf8Path) -> Result<FetchedSource> {
    let checkout = if source.kind == SourceKind::Bundled {
        fetch_checkout(source, &destination.join("bundle"))?
    } else {
        fetch_checkout(source, destination)?
    };
    resolve_fetched_source(source, &checkout)
}

pub fn fetch_source_cached(source: &SourceSpec, cache_dir: &Utf8Path) -> Result<FetchedSource> {
    let checkout = fetch_checkout_cached(source, cache_dir)?;
    resolve_fetched_source(source, &checkout)
}

pub fn fetch_checkout_cached(source: &SourceSpec, cache_dir: &Utf8Path) -> Result<FetchedCheckout> {
    match source.kind {
        SourceKind::Bundled => fetch_bundled_checkout(
            source,
            &cache_dir.join(format!("{}-{}", cache_key(source), self_bundle_digest())),
        ),
        SourceKind::Local => fetch_local_checkout(source),
        SourceKind::Github => git::fetch_checkout_cached(source, cache_dir),
        SourceKind::Owned => Err(Error::Message(
            "locally owned skills have no upstream; use `yasm add` to attach a source".to_string(),
        )),
    }
}

pub fn resolve_fetched_source(
    source: &SourceSpec,
    checkout: &FetchedCheckout,
) -> Result<FetchedSource> {
    Ok(FetchedSource {
        root: source_root(source, &checkout.root)?,
        source: source.clone(),
        resolved: checkout.resolved.clone(),
    })
}

fn fetch_checkout(source: &SourceSpec, destination: &Utf8Path) -> Result<FetchedCheckout> {
    match source.kind {
        SourceKind::Bundled => fetch_bundled_checkout(source, destination),
        SourceKind::Local => fetch_local_checkout(source),
        SourceKind::Github => git::fetch_checkout(source, destination),
        SourceKind::Owned => Err(Error::Message(
            "locally owned skills have no upstream; use `yasm add` to attach a source".to_string(),
        )),
    }
}

fn fetch_bundled_checkout(source: &SourceSpec, destination: &Utf8Path) -> Result<FetchedCheckout> {
    if source.path != SELF_BUNDLE_ID {
        return Err(Error::Message(format!(
            "unknown bundled source `{}`; available bundled sources: {SELF_BUNDLE_ID}",
            source.path
        )));
    }
    let root = bundled::materialize_self_bundle(destination)?;
    Ok(FetchedCheckout {
        root,
        resolved: None,
    })
}

fn fetch_local_checkout(source: &SourceSpec) -> Result<FetchedCheckout> {
    Ok(FetchedCheckout {
        root: Utf8PathBuf::from(source.path.clone()),
        resolved: None,
    })
}

fn source_root(source: &SourceSpec, repository_root: &Utf8Path) -> Result<Utf8PathBuf> {
    let Some(subpath) = &source.subpath else {
        return Ok(repository_root.to_path_buf());
    };
    let root = repository_root.join(subpath.as_str());
    if !root.exists() {
        return Err(Error::Message(format!(
            "GitHub directory `{}` was not found in {} at {}",
            subpath.as_str(),
            source.path,
            source
                .r#ref
                .as_ref()
                .map_or("the default branch", |git_ref| git_ref.as_str())
        )));
    }
    if !root.is_dir() {
        return Err(Error::Message(format!(
            "GitHub tree path `{}` is not a directory in {}",
            subpath.as_str(),
            source.path
        )));
    }
    let canonical_repository = std::fs::canonicalize(repository_root).map_err(|error| {
        Error::Message(format!(
            "failed to resolve cloned repository path {repository_root}: {error}"
        ))
    })?;
    let canonical_root = std::fs::canonicalize(&root).map_err(|error| {
        Error::Message(format!(
            "failed to resolve GitHub directory `{}`: {error}",
            subpath.as_str()
        ))
    })?;
    if !canonical_root.starts_with(&canonical_repository) {
        return Err(Error::Message(format!(
            "GitHub tree path `{}` resolves outside the repository",
            subpath.as_str()
        )));
    }
    Ok(root)
}

pub(crate) fn cache_key(source: &SourceSpec) -> String {
    let mut identity = source.path.clone();
    if let Some(git_ref) = &source.r#ref {
        identity.push('@');
        identity.push_str(git_ref.as_str());
    }
    let name = source
        .path
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("source");
    format!(
        "{:016x}-{}",
        stable_hash(&identity),
        sanitize_cache_name(name)
    )
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn sanitize_cache_name(value: &str) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for ch in value.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            previous_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if previous_dash {
            None
        } else {
            previous_dash = true;
            Some('-')
        };
        if let Some(ch) = next {
            output.push(ch);
        }
    }
    let output = output.trim_matches('-');
    if output.is_empty() {
        "source".to_string()
    } else {
        output.to_string()
    }
}

#[cfg(test)]
mod cache_tests {
    use camino::Utf8PathBuf;
    use tempfile::tempdir;
    use yasm_core::{discover_skills, GitRef, SourceKind};

    use crate::*;

    fn git(dir: &camino::Utf8Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git_text(dir: &camino::Utf8Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn write_skill(root: &camino::Utf8Path, body: &str) {
        let skill_dir = root.join("skills/example");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: example\ndescription: Example\n---\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn cached_git_fetch_reuses_clone_and_pulls_updates() {
        let remote_temp = tempdir().unwrap();
        let remote = Utf8PathBuf::from_path_buf(remote_temp.path().to_path_buf()).unwrap();
        git(&remote, &["init", "-b", "main"]);
        git(&remote, &["config", "user.email", "test@example.com"]);
        git(&remote, &["config", "user.name", "Test"]);
        write_skill(&remote, "old");
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "old"]);

        let cache_temp = tempdir().unwrap();
        let cache = Utf8PathBuf::from_path_buf(cache_temp.path().to_path_buf()).unwrap();
        let source = SourceSpec {
            kind: SourceKind::Github,
            path: remote.as_str().to_string(),
            r#ref: Some(GitRef::parse("main").unwrap()),
            subpath: None,
        };

        let first = fetch_source_cached(&source, &cache).unwrap();
        let first_root = first.root.clone();
        let first_commit = git_text(&remote, &["rev-parse", "HEAD"]);
        assert_eq!(first.resolved.unwrap().commit, first_commit);
        assert!(first_root.join(".git").exists());
        std::fs::write(first_root.join("untracked.tmp"), "cache artifact").unwrap();

        write_skill(&remote, "new");
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "new"]);

        let second = fetch_source_cached(&source, &cache).unwrap();
        assert_eq!(second.root, first_root);
        let second_commit = git_text(&remote, &["rev-parse", "HEAD"]);
        let resolved = second.resolved.unwrap();
        assert_eq!(resolved.r#ref.unwrap().as_str(), "main");
        assert_eq!(resolved.commit, second_commit);
        assert!(!second.root.join("untracked.tmp").exists());
        let content = std::fs::read_to_string(second.root.join("skills/example/SKILL.md")).unwrap();
        assert!(content.contains("new"));
    }

    #[test]
    fn cached_git_fetch_without_ref_follows_remote_default_branch() {
        let remote_temp = tempdir().unwrap();
        let remote = Utf8PathBuf::from_path_buf(remote_temp.path().to_path_buf()).unwrap();
        git(&remote, &["init", "-b", "develop"]);
        git(&remote, &["config", "user.email", "test@example.com"]);
        git(&remote, &["config", "user.name", "Test"]);
        write_skill(&remote, "old");
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "old"]);

        let cache_temp = tempdir().unwrap();
        let cache = Utf8PathBuf::from_path_buf(cache_temp.path().to_path_buf()).unwrap();
        let source = SourceSpec {
            kind: SourceKind::Github,
            path: remote.as_str().to_string(),
            r#ref: None,
            subpath: None,
        };

        let first = fetch_source_cached(&source, &cache).unwrap();
        let first_root = first.root.clone();
        let first_commit = git_text(&remote, &["rev-parse", "HEAD"]);
        let first_resolved = first.resolved.unwrap();
        assert_eq!(first_resolved.r#ref, None);
        assert_eq!(first_resolved.commit, first_commit);
        assert_eq!(
            git_text(&first_root, &["symbolic-ref", "refs/remotes/origin/HEAD"]),
            "refs/remotes/origin/develop"
        );

        write_skill(&remote, "new");
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "new"]);

        let second = fetch_source_cached(&source, &cache).unwrap();
        assert_eq!(second.root, first_root);
        let second_commit = git_text(&remote, &["rev-parse", "HEAD"]);
        let second_resolved = second.resolved.unwrap();
        assert_eq!(second_resolved.r#ref, None);
        assert_eq!(second_resolved.commit, second_commit);
        let content = std::fs::read_to_string(second.root.join("skills/example/SKILL.md")).unwrap();
        assert!(content.contains("new"));
    }

    #[test]
    fn pinned_checkout_stays_at_the_requested_commit() {
        let remote_temp = tempdir().unwrap();
        let remote = Utf8PathBuf::from_path_buf(remote_temp.path().to_path_buf()).unwrap();
        git(&remote, &["init", "-b", "main"]);
        git(&remote, &["config", "user.email", "test@example.com"]);
        git(&remote, &["config", "user.name", "Test"]);
        write_skill(&remote, "pinned");
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "pinned"]);
        let commit = git_text(&remote, &["rev-parse", "HEAD"]);
        let cache_temp = tempdir().unwrap();
        let cache = Utf8PathBuf::from_path_buf(cache_temp.path().to_path_buf()).unwrap();
        let source = SourceSpec {
            kind: SourceKind::Github,
            path: remote.to_string(),
            r#ref: None,
            subpath: None,
        };
        let first = crate::git::fetch_pinned_checkout(&source, &commit, &cache).unwrap();
        assert_eq!(first.resolved.unwrap().commit, commit);
        write_skill(&remote, "new upstream");
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "new"]);
        let second = crate::git::fetch_pinned_checkout(&source, &commit, &cache).unwrap();
        assert_eq!(second.root, first.root);
        assert_eq!(second.resolved.unwrap().commit, commit);
        assert!(
            std::fs::read_to_string(second.root.join("skills/example/SKILL.md"))
                .unwrap()
                .contains("pinned")
        );
    }

    #[test]
    fn git_fetch_scopes_the_root_to_the_requested_subpath() {
        let remote_temp = tempdir().unwrap();
        let remote = Utf8PathBuf::from_path_buf(remote_temp.path().to_path_buf()).unwrap();
        git(&remote, &["init", "-b", "main"]);
        git(&remote, &["config", "user.email", "test@example.com"]);
        git(&remote, &["config", "user.name", "Test"]);
        write_skill(&remote, "selected");
        std::fs::create_dir_all(remote.join("other")).unwrap();
        std::fs::write(
            remote.join("other/SKILL.md"),
            "---\nname: other\ndescription: Other\n---\nbody\n",
        )
        .unwrap();
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "skills"]);

        let destination_temp = tempdir().unwrap();
        let destination =
            Utf8PathBuf::from_path_buf(destination_temp.path().join("clone")).unwrap();
        let source = SourceSpec {
            kind: SourceKind::Github,
            path: remote.as_str().to_string(),
            r#ref: Some(GitRef::parse("main").unwrap()),
            subpath: Some(yasm_core::SkillPath::parse("skills/example").unwrap()),
        };

        let fetched = fetch_source(&source, &destination).unwrap();
        assert_eq!(fetched.root, destination.join("skills/example"));
        let skills = discover_skills(&fetched.root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name.as_str(), "example");
        assert_eq!(skills[0].skill_path.as_str(), "SKILL.md");
    }
}

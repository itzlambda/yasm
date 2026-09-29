mod bundled;
mod source;

use std::io::{self, Read};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use yasm_core::{Error, ResolvedSource, Result, SourceKind, SourceSpec};

const GIT_TIMEOUT: Duration = Duration::from_secs(120);

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
        SourceKind::Github => fetch_git_checkout_cached(source, cache_dir),
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
        SourceKind::Github => fetch_git_checkout(source, destination),
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

fn fetch_git_checkout(source: &SourceSpec, destination: &Utf8Path) -> Result<FetchedCheckout> {
    let mut command = git_command();
    command.arg("clone").arg("--depth").arg("1");
    if let Some(git_ref) = &source.r#ref {
        command.arg("--branch").arg(git_ref.as_str());
    }
    command.arg(&source.path).arg(destination);
    run_git(command, source, "git clone")?;

    Ok(FetchedCheckout {
        root: destination.to_path_buf(),
        resolved: Some(resolve_git_source(source, destination)?),
    })
}

fn fetch_git_checkout_cached(source: &SourceSpec, cache_dir: &Utf8Path) -> Result<FetchedCheckout> {
    std::fs::create_dir_all(cache_dir).map_err(|err| {
        Error::Message(format!(
            "failed to create source cache {}: {err}",
            cache_dir
        ))
    })?;
    let destination = cache_dir.join(cache_key(source));
    if destination.join(".git").exists() {
        update_cached_git(source, &destination)?;
    } else if destination.exists() {
        return Err(Error::Message(format!(
            "source cache path exists but is not a git repository: {destination}"
        )));
    } else {
        fetch_git_checkout(source, &destination)?;
    }

    let resolved = resolve_git_source(source, &destination)?;
    Ok(FetchedCheckout {
        root: destination,
        resolved: Some(resolved),
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

fn resolve_git_source(source: &SourceSpec, destination: &Utf8Path) -> Result<ResolvedSource> {
    let mut command = git_command();
    command
        .arg("-C")
        .arg(destination)
        .arg("rev-parse")
        .arg("HEAD");
    let commit = git_stdout(command, source, "git rev-parse")?;
    Ok(ResolvedSource {
        r#ref: source.r#ref.clone(),
        commit: commit.trim().to_string(),
    })
}

fn update_cached_git(source: &SourceSpec, destination: &Utf8Path) -> Result<()> {
    let ref_name = source.r#ref.as_ref().map(|git_ref| git_ref.as_str());
    let mut fetch = git_command();
    fetch
        .arg("-C")
        .arg(destination)
        .arg("fetch")
        .arg("--depth")
        .arg("1")
        .arg("origin");
    if let Some(ref_name) = ref_name {
        fetch.arg(ref_name);
    }
    run_git(fetch, source, "git fetch")?;

    let checkout_target = if ref_name.is_some() {
        "FETCH_HEAD"
    } else {
        "origin/HEAD"
    };
    let mut checkout = git_command();
    checkout
        .arg("-C")
        .arg(destination)
        .arg("checkout")
        .arg("--force")
        .arg(checkout_target);
    run_git(checkout, source, "git checkout")?;

    let mut clean = git_command();
    clean.arg("-C").arg(destination).arg("clean").arg("-fdx");
    run_git(clean, source, "git clean")?;
    Ok(())
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("credential.helper=")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
    command
}

fn run_git(command: Command, source: &SourceSpec, action: &str) -> Result<()> {
    let output = run_git_output(command, source, action)?;
    if !output.status.success() {
        return Err(unsuccessful_git_output(&output, source, action));
    }
    Ok(())
}

fn git_stdout(command: Command, source: &SourceSpec, action: &str) -> Result<String> {
    let output = run_git_output(command, source, action)?;
    if !output.status.success() {
        return Err(unsuccessful_git_output(&output, source, action));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_git_output(command: Command, source: &SourceSpec, action: &str) -> Result<Output> {
    run_command_with_timeout(command, GIT_TIMEOUT)
        .map_err(|error| command_error(error, source, action))
}

fn unsuccessful_git_output(output: &Output, source: &SourceSpec, action: &str) -> Error {
    let status = exit_status_label(&output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = if stderr.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        stderr
    };
    let detail = detail.trim();
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    Error::Message(format!(
        "{action} failed for {} ({status}){suffix}",
        source.path
    ))
}

fn exit_status_label(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => status.to_string(),
    }
}

fn command_error(error: CommandError, source: &SourceSpec, action: &str) -> Error {
    match error {
        CommandError::Spawn(source) => Error::SubprocessIo {
            action: action.to_string(),
            operation: "spawn git (ensure git is installed on PATH)",
            source,
        },
        CommandError::Read(stream, source) => Error::SubprocessIo {
            action: action.to_string(),
            operation: stream.read_operation(),
            source,
        },
        CommandError::Wait(source) => Error::SubprocessIo {
            action: action.to_string(),
            operation: "wait",
            source,
        },
        CommandError::ReaderPanic(stream) => Error::Message(format!(
            "{action} {} reader thread panicked",
            stream.name()
        )),
        CommandError::Timeout => Error::Message(format!(
            "{action} timed out after {}s for {}; yasm does not wait indefinitely for git. Check the URL and network",
            GIT_TIMEOUT.as_secs(),
            source.path
        )),
    }
}

#[derive(Debug)]
enum CommandError {
    Spawn(io::Error),
    Timeout,
    Read(Stream, io::Error),
    Wait(io::Error),
    ReaderPanic(Stream),
}

#[derive(Debug, Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }

    fn read_operation(self) -> &'static str {
        match self {
            Self::Stdout => "read stdout",
            Self::Stderr => "read stderr",
        }
    }
}

fn run_command_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> std::result::Result<Output, CommandError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(CommandError::Spawn)?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_handle = std::thread::spawn(move || read_pipe(stderr_pipe));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return Err(CommandError::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return Err(CommandError::Wait(err));
            }
        }
    };

    let stdout = join_reader(stdout_handle, Stream::Stdout);
    let stderr = join_reader(stderr_handle, Stream::Stderr);
    Ok(Output {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

fn join_reader(
    handle: std::thread::JoinHandle<io::Result<Vec<u8>>>,
    stream: Stream,
) -> std::result::Result<Vec<u8>, CommandError> {
    handle
        .join()
        .map_err(|_| CommandError::ReaderPanic(stream))?
        .map_err(|error| CommandError::Read(stream, error))
}

fn read_pipe<T: Read>(pipe: Option<T>) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut pipe = pipe.ok_or_else(|| io::Error::other("subprocess pipe unavailable"))?;
    pipe.read_to_end(&mut buf)?;
    Ok(buf)
}

fn cache_key(source: &SourceSpec) -> String {
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
mod command_timeout_tests {
    use std::io::{self, Read};
    use std::time::{Duration, Instant};

    use super::*;

    struct FailingReader(bool);

    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            if !self.0 {
                self.0 = true;
                buf[0] = b'x';
                return Ok(1);
            }
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader failed"))
        }
    }

    fn test_source() -> SourceSpec {
        SourceSpec {
            kind: SourceKind::Github,
            path: "https://example.com/repo.git".to_string(),
            r#ref: None,
            subpath: None,
        }
    }

    #[test]
    fn pipe_read_failure_is_preserved() {
        let error = read_pipe(Some(FailingReader(false))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "reader failed");
    }

    #[test]
    fn missing_pipe_is_an_error() {
        let error = read_pipe(None::<io::Empty>).unwrap_err();
        assert!(error.to_string().contains("pipe unavailable"));
    }

    #[test]
    fn reader_thread_failure_is_preserved() {
        let read_failure = std::thread::spawn(|| read_pipe(Some(FailingReader(false))));
        assert!(matches!(
            join_reader(read_failure, Stream::Stderr),
            Err(CommandError::Read(Stream::Stderr, error)) if error.kind() == io::ErrorKind::BrokenPipe
        ));

        let panic_failure =
            std::thread::spawn(|| -> io::Result<Vec<u8>> { panic!("reader panic") });
        assert!(matches!(
            join_reader(panic_failure, Stream::Stdout),
            Err(CommandError::ReaderPanic(Stream::Stdout))
        ));
    }

    #[test]
    fn read_and_wait_failures_retain_sources_in_returned_errors() {
        let read_handle = std::thread::spawn(|| read_pipe(Some(FailingReader(false))));
        let read_failure = join_reader(read_handle, Stream::Stdout).unwrap_err();
        let cases = [
            (
                read_failure,
                "read stdout",
                io::ErrorKind::BrokenPipe,
                "reader failed",
            ),
            (
                CommandError::Wait(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "wait failed",
                )),
                "wait",
                io::ErrorKind::PermissionDenied,
                "wait failed",
            ),
        ];

        for (failure, operation, kind, detail) in cases {
            let error = command_error(failure, &test_source(), "git fetch");
            assert!(error.to_string().contains("git fetch"));
            assert!(error.to_string().contains(operation));
            assert!(!error.to_string().contains("ensure git is installed"));
            let source = std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<io::Error>())
                .expect("returned error should retain the I/O source");
            assert_eq!(source.kind(), kind);
            assert_eq!(source.to_string(), detail);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsuccessful_subprocess_keeps_stderr() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf 'stdout detail'; printf 'specific failure' >&2; exit 7");
        let error = run_git(command, &test_source(), "git fetch").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("git fetch failed"));
        assert!(message.contains("exit status 7"));
        assert!(message.contains("specific failure"));
        assert!(!message.contains("stdout detail"));
    }

    #[cfg(unix)]
    #[test]
    fn unsuccessful_subprocess_uses_stdout_when_stderr_is_empty() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf 'stdout detail'; exit 4");
        let error = run_git(command, &test_source(), "git fetch").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("exit status 4"));
        assert!(message.contains("stdout detail"));
    }

    #[cfg(unix)]
    #[test]
    fn unsuccessful_subprocess_without_output_reports_status() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 9");
        let error = run_git(command, &test_source(), "git fetch").unwrap_err();
        assert_eq!(
            error.to_string(),
            "git fetch failed for https://example.com/repo.git (exit status 9)"
        );
    }

    #[test]
    fn spawn_failure_keeps_source_and_install_hint() {
        let command = Command::new("yasm-command-that-does-not-exist");
        let error = run_git_output(command, &test_source(), "git fetch").unwrap_err();
        assert!(error
            .to_string()
            .contains("ensure git is installed on PATH"));
        assert!(
            matches!(error, Error::SubprocessIo { operation: "spawn git (ensure git is installed on PATH)", source, .. } if source.kind() == io::ErrorKind::NotFound)
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout_kills_the_process() {
        let mut command = Command::new("sleep");
        command.arg("10");
        let started = Instant::now();
        let error = run_command_with_timeout(command, Duration::from_millis(200)).unwrap_err();
        assert!(matches!(error, CommandError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn command_does_not_block_on_stdin() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("read -r line || true; printf done");
        let started = Instant::now();
        let output = run_command_with_timeout(command, Duration::from_secs(2)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(output.status.success());
        assert_eq!(output.stdout, b"done");
    }

    #[test]
    fn github_clone_of_missing_repo_fails_without_waiting_for_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let destination =
            Utf8PathBuf::from_path_buf(temp.path().join("clone")).expect("utf-8 temp path");
        let source = SourceSpec {
            kind: SourceKind::Github,
            path: "https://github.com/yasm/this-repo-does-not-exist.git".to_string(),
            r#ref: None,
            subpath: None,
        };
        let started = Instant::now();
        let error = match fetch_git_checkout(&source, &destination) {
            Ok(_) => panic!("missing GitHub repository should not clone"),
            Err(error) => error,
        };
        if started.elapsed() >= Duration::from_secs(30) {
            panic!("clone of missing GitHub repository hung: {error}");
        }
        let message = error.to_string();
        assert!(
            !message.contains("timed out"),
            "missing repository should fail immediately, got: {message}"
        );
        assert!(
            message.contains("git clone failed")
                || message.contains("not found")
                || message.contains("Authentication")
                || message.contains("could not read Username")
                || message.contains("terminal prompts disabled")
                || message.contains("repository"),
            "unexpected clone error: {message}"
        );
    }
}

#[cfg(test)]
mod cache_tests {
    use camino::Utf8PathBuf;
    use tempfile::tempdir;
    use yasm_core::{discover_skills, GitRef, SourceKind};

    use super::*;

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

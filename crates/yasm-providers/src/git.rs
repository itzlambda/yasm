//! Shared Git checkout and subprocess handling for skill and marketplace sources.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use sha2::{Digest, Sha256};
use yasm_core::{Error, ResolvedSource, Result, SourceKind, SourceSpec};

use crate::{cache_key, FetchedCheckout};

const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Keep a managed checkout locked while callers inspect or copy its contents.
#[derive(Debug, Clone)]
pub struct CheckoutLease {
    _file: Arc<File>,
}

fn checkout_lease(cache_dir: &Utf8Path, key: &str) -> Result<CheckoutLease> {
    let path = cache_dir.join(format!("{key}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| yasm_core::error::io(&path, error))?;
    file.try_lock().map_err(|error| Error::Message(format!(
        "cannot lock Git checkout cache; another Yasm command may be using it; retry after that command finishes: {error}"
    )))?;
    Ok(CheckoutLease {
        _file: Arc::new(file),
    })
}

fn managed_checkout_exists(destination: &Utf8Path) -> Result<bool> {
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            let git_dir = destination.join(".git");
            if std::fs::symlink_metadata(&git_dir)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            {
                Ok(true)
            } else {
                Err(Error::Message(format!(
                    "source cache exists but is not a managed Git repository: {destination}"
                )))
            }
        }
        Ok(_) => Err(Error::Message(format!(
            "source cache exists but is not a managed Git repository: {destination}"
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(yasm_core::error::io(destination, error)),
    }
}

/// A repository address validated independently of skill or catalog inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRemote {
    address: String,
    transport: GitTransport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitTransport {
    Https,
    Ssh,
}

impl GitRemote {
    pub fn as_str(&self) -> &str {
        &self.address
    }

    pub fn transport(&self) -> GitTransport {
        self.transport
    }
}

impl From<GitRemote> for SourceSpec {
    fn from(remote: GitRemote) -> Self {
        Self {
            kind: match remote.transport {
                GitTransport::Https => SourceKind::Github,
                GitTransport::Ssh => SourceKind::Git,
            },
            path: remote.address,
            r#ref: None,
            subpath: None,
        }
    }
}

impl std::str::FromStr for GitRemote {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self> {
        let invalid = || {
            Error::Message(
                concat!(
            "invalid Git remote; use a GitHub HTTPS repository URL or SCP-style user@host:path; ",
            "credentials, URL queries and fragments are not allowed"
        )
                .to_string(),
            )
        };
        if let Some(path) = input.strip_prefix("https://github.com/") {
            let parts: Vec<_> = path.trim_end_matches('/').split('/').collect();
            if parts.len() != 2 || !parts.iter().all(|part| valid_repo_part(part)) {
                return Err(invalid());
            }
            let repository = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
            if !valid_repo_part(repository) {
                return Err(invalid());
            }
            return Ok(Self {
                address: format!("https://github.com/{}/{repository}.git", parts[0]),
                transport: GitTransport::Https,
            });
        }
        if input.contains("://") {
            return Err(invalid());
        }
        let Some((authority, path)) = input.split_once(':') else {
            return Err(invalid());
        };
        let Some((user, host)) = authority.split_once('@') else {
            return Err(invalid());
        };
        if !valid_repo_part(user)
            || !valid_repo_part(host)
            || user.starts_with('-')
            || host.starts_with('-')
            || path.is_empty()
            || path.starts_with('-')
            || !path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-/".contains(&byte))
            || path
                .trim_start_matches('/')
                .split('/')
                .any(|part| !valid_repo_part(part))
        {
            return Err(invalid());
        }
        Ok(Self {
            address: input.to_string(),
            transport: GitTransport::Ssh,
        })
    }
}

pub fn fetch_checkout(source: &SourceSpec, destination: &Utf8Path) -> Result<FetchedCheckout> {
    let cleanup_on_failure = !destination.exists()
        || std::fs::read_dir(destination).is_ok_and(|mut entries| entries.next().is_none());
    let mut command = git_command();
    command.arg("clone").arg("--depth").arg("1");
    if let Some(git_ref) = &source.r#ref {
        command.arg("--branch").arg(git_ref.as_str());
    }
    command.arg("--").arg(&source.path).arg(destination);
    if let Err(error) = run_git(command, source, "git clone") {
        // Callers only clone into empty staging/cache destinations owned by Yasm.
        // Git may leave a partial clone after a timeout or authentication failure.
        if cleanup_on_failure {
            let _ = std::fs::remove_dir_all(destination);
        }
        return Err(error);
    }

    Ok(FetchedCheckout {
        root: destination.to_path_buf(),
        resolved: Some(resolve_git_source(source, destination)?),
        lease: None,
    })
}

pub fn fetch_checkout_cached(source: &SourceSpec, cache_dir: &Utf8Path) -> Result<FetchedCheckout> {
    std::fs::create_dir_all(cache_dir).map_err(|err| {
        Error::Message(format!(
            "failed to create source cache {}: {err}",
            cache_dir
        ))
    })?;
    let key = cache_key(source);
    let lease = checkout_lease(cache_dir, &key)?;
    let destination = cache_dir.join(key);
    if managed_checkout_exists(&destination)? {
        update_cached_git(source, &destination)?;
    } else {
        fetch_checkout(source, &destination)?;
    }

    let resolved = resolve_git_source(source, &destination)?;
    Ok(FetchedCheckout {
        root: destination,
        resolved: Some(resolved),
        lease: Some(lease),
    })
}

/// Fetch a full commit pin into the caller's pinned-checkout cache.
pub fn fetch_pinned_checkout(
    source: &SourceSpec,
    sha: &str,
    cache_dir: &Utf8Path,
) -> Result<FetchedCheckout> {
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Message(format!(
            "invalid Git commit pin `{sha}`; expected a full 40-character commit SHA"
        )));
    }
    let key = format!(
        "{:x}",
        Sha256::digest(format!("{}\0{sha}", source.path).as_bytes())
    );
    std::fs::create_dir_all(cache_dir).map_err(|error| yasm_core::error::io(cache_dir, error))?;
    let lease = checkout_lease(cache_dir, &key)?;
    let destination = cache_dir.join(key);
    if !managed_checkout_exists(&destination)? {
        let mut clone = git_command();
        clone
            .args([
                "clone",
                "--no-checkout",
                "--filter=blob:none",
                "--",
                &source.path,
            ])
            .arg(&destination);
        if let Err(error) = run_git(clone, source, "clone pinned plugin source") {
            let _ = std::fs::remove_dir_all(&destination);
            return Err(error);
        }
    }
    let mut fetch = git_command();
    fetch
        .arg("-C")
        .arg(&destination)
        .args(["fetch", "--depth", "1", "origin", sha]);
    run_git(fetch, source, "fetch pinned plugin commit")?;
    let mut checkout = git_command();
    checkout
        .arg("-C")
        .arg(&destination)
        .args(["checkout", "--detach", "--force", sha]);
    run_git(checkout, source, "check out pinned plugin commit")?;
    let resolved = resolve_git_source(source, &destination)?;
    Ok(FetchedCheckout {
        root: destination,
        resolved: Some(resolved),
        lease: Some(lease),
    })
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

fn run_git_output(mut command: Command, source: &SourceSpec, action: &str) -> Result<Output> {
    if source.kind == SourceKind::Git {
        let remote: GitRemote = source.path.parse()?;
        if remote.transport() == GitTransport::Ssh {
            let clone_context = clone_destination(&command)
                .map(|destination| CloneConfigContext::prepare(destination, source))
                .transpose()?;
            configure_ssh(&mut command, source)?;
            if let Some(context) = clone_context {
                context.close()?;
            }
        }
    }
    run_command_with_timeout(command, GIT_TIMEOUT)
        .map_err(|error| command_error(error, source, action))
}

fn clone_destination(command: &Command) -> Option<&std::ffi::OsStr> {
    let mut arguments = command.get_args();
    while let Some(option) = arguments.next() {
        if option == "-c" || option == "-C" {
            arguments.next();
        } else {
            return (option == "clone").then(|| arguments.last()).flatten();
        }
    }
    None
}

// Git clone reads destination-dependent includeIf settings after initializing .git.
// Supply that context for our SSH preflight, then remove it before the real clone.
struct CloneConfigContext {
    git_dir: std::path::PathBuf,
}

impl CloneConfigContext {
    fn prepare(destination: &std::ffi::OsStr, source: &SourceSpec) -> Result<Self> {
        let destination = std::path::Path::new(destination);
        std::fs::create_dir_all(destination).map_err(|error| {
            Error::Message(format!(
                "cannot prepare clone configuration context: {error}"
            ))
        })?;
        let git_dir = destination.join(".git");
        // Never adopt or remove metadata that we did not create.
        std::fs::create_dir(&git_dir).map_err(|error| {
            Error::Message(format!(
                "cannot prepare clone configuration context: {error}"
            ))
        })?;
        let context = Self { git_dir };
        let mut init = git_command();
        init.args(["init", "--bare", "--quiet", "--"])
            .arg(&context.git_dir);
        let output = run_command_with_timeout(init, GIT_TIMEOUT)
            .map_err(|error| command_error(error, source, "git init"))?;
        if !output.status.success() {
            return Err(unsuccessful_git_output(&output, source, "git init"));
        }
        Ok(context)
    }

    fn close(self) -> Result<()> {
        std::fs::remove_dir_all(&self.git_dir).map_err(|error| {
            Error::Message(format!("cannot clean clone configuration context: {error}"))
        })
    }
}

impl Drop for CloneConfigContext {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.git_dir);
    }
}

// Read user Git settings with the same bounded subprocess runner, without SSH setup recursion.
fn git_setting(name: &str, source: &SourceSpec, context: &Command) -> Result<Option<String>> {
    let mut command = git_command();
    if let Some(directory) = context.get_current_dir() {
        command.current_dir(directory);
    }
    // Mirror the global options used by our Git builder before its subcommand.
    // In particular, -C determines local config and includeIf gitdir matching.
    let mut arguments = context.get_args();
    while let Some(option) = arguments.next() {
        if option != "-C" && option != "-c" {
            break;
        }
        if let Some(value) = arguments.next() {
            command.arg(option).arg(value);
        }
    }
    if let Some(destination) = clone_destination(context) {
        command
            .arg("--git-dir")
            .arg(std::path::Path::new(destination).join(".git"));
    }
    command.args(["config", "--get", name]);
    let output = run_command_with_timeout(command, GIT_TIMEOUT)
        .map_err(|error| command_error(error, source, "git config"))?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Some(1) => Ok(None),
        _ => Err(unsuccessful_git_output(&output, source, "git config")),
    }
}

fn configure_ssh(command: &mut Command, source: &SourceSpec) -> Result<()> {
    let variant = match std::env::var("GIT_SSH_VARIANT") {
        Ok(variant) => Some(variant),
        Err(_) => git_setting("ssh.variant", source, command)?,
    };
    if variant
        .as_deref()
        .is_some_and(|variant| variant != "ssh" && variant != "auto")
    {
        return Err(Error::Message("SCP-style sources require an OpenSSH-compatible command; set GIT_SSH_VARIANT=ssh and configure keys in ~/.ssh/config".to_string()));
    }
    let words = if let Ok(configured) = std::env::var("GIT_SSH_COMMAND") {
        parse_ssh_command(&configured)?
    } else if let Some(configured) = git_setting("core.sshCommand", source, command)? {
        parse_ssh_command(&configured)?
    } else if let Ok(executable) = std::env::var("GIT_SSH") {
        vec![executable]
    } else {
        vec!["ssh".to_string()]
    };
    let ssh_command = noninteractive_ssh_command(&words)?;
    command
        .env("GIT_SSH_COMMAND", ssh_command)
        .env("GIT_SSH_VARIANT", "ssh")
        .env("SSH_ASKPASS_REQUIRE", "never");
    Ok(())
}

fn parse_ssh_command(configured: &str) -> Result<Vec<String>> {
    // Accept executable/argument commands, not shell expressions that would bypass our options.
    if configured.contains(['\n', '\r', ';', '|', '&', '$', '`', '<', '>']) {
        return Err(Error::Message("SSH command must be an executable with arguments; move shell logic into an OpenSSH-compatible wrapper".to_string()));
    }
    shlex::split(configured)
        .filter(|words| !words.is_empty())
        .ok_or_else(|| {
            Error::Message(
                "invalid SSH command; configure an OpenSSH-compatible executable with arguments"
                    .to_string(),
            )
        })
}

fn noninteractive_ssh_command(words: &[String]) -> Result<String> {
    let Some((executable, arguments)) = words.split_first() else {
        return Err(Error::Message("SSH command is empty".to_string()));
    };
    if executable.is_empty() {
        return Err(Error::Message("SSH executable is empty".to_string()));
    }
    // OpenSSH uses the first supplied value, so insert required options before user arguments.
    let mut command = vec![
        executable.as_str(),
        "-oBatchMode=yes",
        "-oStrictHostKeyChecking=yes",
    ];
    command.extend(arguments.iter().map(String::as_str));
    shlex::try_join(command).map_err(|_| Error::Message("invalid SSH command".to_string()))
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
    let hint = if source.kind == SourceKind::Git {
        "; check repository access, available SSH keys/agent, and trusted host keys with Git outside Yasm"
    } else {
        ""
    };
    Error::Message(format!(
        "{action} failed for {} ({status}){suffix}{hint}",
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

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(CommandError::Spawn)?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_handle = std::thread::spawn(move || read_pipe(stderr_pipe));

    let deadline = Instant::now() + timeout;
    let mut exit_status = None;
    let status = loop {
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(status) => exit_status = status,
                Err(error) => {
                    terminate_process_tree(&mut child);
                    let _ = child.wait();
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    return Err(CommandError::Wait(error));
                }
            }
        }
        if let Some(status) = exit_status {
            if stdout_handle.is_finished() && stderr_handle.is_finished() {
                break status;
            }
        }
        if Instant::now() >= deadline {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Err(CommandError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = join_reader(stdout_handle, Stream::Stdout);
    let stderr = join_reader(stderr_handle, Stream::Stderr);
    Ok(Output {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
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

pub(crate) fn valid_repo_part(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
}

#[cfg(test)]
mod command_timeout_tests {
    use std::io::{self, Read};
    use std::time::{Duration, Instant};

    use camino::Utf8PathBuf;
    use yasm_core::SourceKind;

    use crate::git::*;

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
        let error = match fetch_checkout(&source, &destination) {
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
mod ssh_tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use crate::git::{
        noninteractive_ssh_command, parse_ssh_command, run_command_with_timeout, CommandError,
        GitRemote, GitTransport,
    };

    #[test]
    fn scp_addresses_preserve_aliases_and_nested_paths() {
        for address in [
            "git@github.com:team/repo.git",
            "git@work-alias:group/team/repo",
            "user@git.example.test:/srv/git/repo.git",
        ] {
            let remote: GitRemote = address.parse().unwrap();
            assert_eq!(remote.as_str(), address);
            assert_eq!(remote.transport(), GitTransport::Ssh);
        }
    }

    #[test]
    fn invalid_remotes_never_echo_credentials() {
        for address in [
            "ssh://git@github.com/team/repo",
            "https://user:top-secret@github.com/team/repo",
            "git:top-secret@host:repo",
            "git@host:repo?token=top-secret",
            "git@host:repo;touch-file",
            "git@host:-repo",
            "-git@host:repo",
            "git@-host:repo",
            "git@host:group/../repo",
            "git@host:",
            "git@:repo",
            "https://github.com/team/repo?token=top-secret",
        ] {
            let error = address.parse::<GitRemote>().unwrap_err();
            assert!(!format!("{error:?} {error}").contains("top-secret"));
        }
    }

    #[test]
    fn ssh_policy_precedes_user_options_and_quotes_executable_paths() {
        let words = parse_ssh_command(
            "'/path with spaces/ssh' -i 'key file' -oBatchMode=no -oStrictHostKeyChecking=no",
        )
        .unwrap();
        let command = noninteractive_ssh_command(&words).unwrap();
        let arguments = shlex::split(&command).unwrap();
        assert_eq!(
            &arguments[..3],
            &[
                "/path with spaces/ssh",
                "-oBatchMode=yes",
                "-oStrictHostKeyChecking=yes"
            ]
        );
        assert_eq!(&arguments[3..5], &["-i", "key file"]);
    }

    #[test]
    fn ssh_shell_expressions_require_a_wrapper() {
        for command in ["", "ssh; echo bad", "ssh $(echo bad)", "ssh | cat"] {
            assert!(parse_ssh_command(command).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_descendants_even_after_parent_exits() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10 & exit 0"]);
        let started = Instant::now();
        let result = run_command_with_timeout(command, Duration::from_millis(200));
        assert!(matches!(result, Err(CommandError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

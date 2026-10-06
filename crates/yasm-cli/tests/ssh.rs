#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
#[cfg(feature = "marketplace")]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const SOURCE: &str = "git@work-alias:team/private-skills.git";

struct Sandbox {
    root: TempDir,
    remote: PathBuf,
    ssh: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote");
        let ssh = root.path().join("fake ssh");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        std::fs::write(
            &ssh,
            r#"#!/bin/sh
set -eu
printf '%s\n' "$@" >> "$FAKE_SSH_LOG"
batch=no
verify=no
for arg in "$@"; do
  case "$arg" in
    -oBatchMode=yes) batch=yes ;;
    -oStrictHostKeyChecking=yes) verify=yes ;;
  esac
done
[ "$batch" = yes ] && [ "$verify" = yes ] || exit 99
[ "${SSH_ASKPASS_REQUIRE:-}" = never ] || exit 98
if [ "${FAKE_SSH_FAILURE:-}" = yes ]; then
  printf 'Permission denied (publickey).\n' >&2
  exit 255
fi
exec git-upload-pack "$FAKE_SSH_REPO"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sandbox = Self { root, remote, ssh };
        sandbox.git(&["init", "-b", "main"]);
        sandbox.git(&["config", "user.email", "test@example.test"]);
        sandbox.git(&["config", "user.name", "Test"]);
        sandbox
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_yasm"));
        command
            .current_dir(self.root.path())
            .env("HOME", self.root.path().join("home"))
            .env("YASM_DATA_DIR", self.root.path().join("data"))
            .env("YASM_CACHE_DIR", self.root.path().join("cache"))
            .env("YASM_CONFIG_DIR", self.root.path().join("config"))
            .env("YASM_AGENT_SKILLS_ROOT", self.root.path().join("agents"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_SSH", &self.ssh)
            .env_remove("GIT_SSH_COMMAND")
            .env("GIT_SSH_VARIANT", "ssh")
            .env("FAKE_SSH_REPO", &self.remote)
            .env("FAKE_SSH_LOG", self.root.path().join("ssh.log"))
            .env_remove("CODEX_HOME");
        command
    }

    fn success(&self, args: &[&str]) -> Output {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{:?}: {} {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.remote)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn write(&self, path: &str, body: &str) {
        let path = self.remote.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn commit(&self) -> String {
        self.git(&["add", "."]);
        self.git(&["commit", "-m", "fixture"]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn lock(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.root.path().join("data/yasm.lock")).unwrap())
            .unwrap()
    }
}

#[test]
fn scp_root_skill_add_update_and_failure_preserve_provenance_and_files() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "SKILL.md",
        "---\nname: review\ndescription: Review\n---\nold\n",
    );
    let old_commit = sandbox.commit();
    sandbox.success(&["add", SOURCE, "--global", "--action", "apply"]);
    let installed = sandbox.root.path().join("data/skills/review");
    assert!(!installed.join(".git").exists());
    let lock = sandbox.lock();
    assert_eq!(lock["skills"]["review"]["source"]["kind"], "git");
    assert_eq!(lock["skills"]["review"]["source"]["path"], SOURCE);
    assert_eq!(lock["skills"]["review"]["resolved"]["commit"], old_commit);
    let info = sandbox.success(&["info", "review", "--global"]);
    assert!(String::from_utf8_lossy(&info.stdout).contains("git@work-alias:team/private-skills"));

    sandbox.write(
        "SKILL.md",
        "---\nname: review\ndescription: Review\n---\nnew\n",
    );
    let new_commit = sandbox.commit();
    sandbox.success(&["update", "review", "--global", "--action", "apply"]);
    assert!(std::fs::read_to_string(installed.join("SKILL.md"))
        .unwrap()
        .contains("new"));
    assert_eq!(
        sandbox.lock()["skills"]["review"]["resolved"]["commit"],
        new_commit
    );
    assert!(!installed.join(".git").exists());
    // Exercise the reused checkout, not just a fresh clone.
    sandbox.success(&["update", "review", "--global", "--action", "apply"]);

    let before = std::fs::read(sandbox.root.path().join("data/yasm.lock")).unwrap();
    let failed = sandbox
        .command()
        .env("FAKE_SSH_FAILURE", "yes")
        .args([
            "update", "review", "--global", "--action", "apply", "--json",
        ])
        .output()
        .unwrap();
    let diagnostics = format!(
        "{} {}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(
        diagnostics.contains("Permission denied (publickey)"),
        "{diagnostics}"
    );
    assert!(diagnostics.contains("SSH keys/agent"), "{diagnostics}");
    assert_eq!(
        std::fs::read(sandbox.root.path().join("data/yasm.lock")).unwrap(),
        before
    );
    assert!(std::fs::read_to_string(installed.join("SKILL.md"))
        .unwrap()
        .contains("new"));
}

#[test]
fn scp_migration_recovers_git_and_github_install_history() {
    for (source_type, source_url) in [
        ("git", Some(SOURCE)),
        ("github", Some(SOURCE)),
        ("git", None),
    ] {
        let sandbox = Sandbox::new();
        let installed = sandbox.root.path().join("agents/.agents/skills/review");
        std::fs::create_dir_all(&installed).unwrap();
        let content = "---\nname: review\ndescription: Review\n---\nlocal\n";
        std::fs::write(installed.join("SKILL.md"), content).unwrap();
        let state = sandbox.root.path().join("state");
        let history = state.join("skills/.skill-lock.json");
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(
            &history,
            serde_json::json!({
                "version": 3,
                "skills": {"review": {
                    "source": SOURCE,
                    "sourceUrl": source_url,
                    "sourceType": source_type,
                    "skillPath": "skills/review/SKILL.md",
                    "ref": "release"
                }}
            })
            .to_string(),
        )
        .unwrap();
        let output = sandbox
            .command()
            .env("XDG_STATE_HOME", state)
            .args([
                "migrate",
                "--global",
                "--skill",
                "review",
                "--with-upstream",
                "--action",
                "apply",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let lock = sandbox.lock();
        let source = &lock["skills"]["review"]["source"];
        assert_eq!(source["kind"], "git");
        assert_eq!(source["path"], SOURCE);
        assert_eq!(source["subpath"], "skills/review");
        assert_eq!(source["ref"], "release");
        assert_eq!(
            std::fs::read_to_string(sandbox.root.path().join("data/skills/review/SKILL.md"))
                .unwrap(),
            content
        );
        assert!(history.exists());
    }
}

#[test]
fn scp_migration_attaches_an_upstream_without_replacing_installed_content() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "skills/review/SKILL.md",
        "---\nname: review\ndescription: Review\n---\nupstream\n",
    );
    sandbox.commit();
    let installed = sandbox.root.path().join("agents/.agents/skills/review");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(
        installed.join("SKILL.md"),
        "---\nname: review\ndescription: Review\n---\nlocal\n",
    )
    .unwrap();
    sandbox.success(&[
        "migrate", "--global", "--skill", "review", "--source", SOURCE, "--action", "apply",
    ]);
    assert_eq!(sandbox.lock()["skills"]["review"]["source"]["path"], SOURCE);
    assert_eq!(
        sandbox.lock()["skills"]["review"]["source"]["subpath"],
        "skills/review"
    );
    assert!(
        std::fs::read_to_string(sandbox.root.path().join("data/skills/review/SKILL.md"))
            .unwrap()
            .contains("local")
    );
    sandbox.success(&["update", "review", "--global", "--action", "apply"]);
    assert!(
        std::fs::read_to_string(sandbox.root.path().join("data/skills/review/SKILL.md"))
            .unwrap()
            .contains("upstream")
    );
}

#[test]
fn scp_authentication_failure_is_noninteractive_and_retryable() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "SKILL.md",
        "---\nname: review\ndescription: Review\n---\nbody\n",
    );
    sandbox.commit();
    let started = std::time::Instant::now();
    let failed = sandbox
        .command()
        .env("FAKE_SSH_FAILURE", "yes")
        .args(["add", SOURCE, "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(!sandbox.root.path().join("data/skills/review").exists());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("trusted host keys"));
    sandbox.success(&["add", SOURCE, "--global", "--action", "apply"]);
}

#[test]
fn scp_respects_ssh_command_override_and_rejects_unsupported_variants() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "SKILL.md",
        "---\nname: review\ndescription: Review\n---\nbody\n",
    );
    sandbox.commit();
    let command = format!("'{}' -i 'key file'", sandbox.ssh.display());
    let output = sandbox
        .command()
        .env("GIT_SSH_COMMAND", command)
        .env("GIT_SSH", "missing-ssh")
        .args(["add", SOURCE, "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(std::fs::read_to_string(sandbox.root.path().join("ssh.log"))
        .unwrap()
        .contains("key file"));
    let rejected = sandbox
        .command()
        .env("GIT_SSH_VARIANT", "plink")
        .args([
            "update", "review", "--global", "--action", "apply", "--json",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&rejected.stdout).contains("OpenSSH-compatible"));
}

#[test]
fn scp_respects_core_ssh_command_before_git_ssh() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "SKILL.md",
        "---\nname: review\ndescription: Review\n---\nbody\n",
    );
    sandbox.commit();
    let command = format!("'{}' -i 'configured key'", sandbox.ssh.display());
    let configured = Command::new("git")
        .args(["config", "--file"])
        .arg(sandbox.root.path().join("home/.gitconfig"))
        .args(["core.sshCommand", &command])
        .status()
        .unwrap();
    assert!(configured.success());
    let output = sandbox
        .command()
        .env("GIT_SSH", "missing-ssh")
        .args(["add", SOURCE, "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(std::fs::read_to_string(sandbox.root.path().join("ssh.log"))
        .unwrap()
        .contains("configured key"));
}

#[test]
fn scp_git_operations_respect_repository_configuration_context() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "SKILL.md",
        "---\nname: review\ndescription: Review\n---\nbody\n",
    );
    sandbox.commit();
    // The caller's unrelated repository configuration must not affect cached Git operations.
    let caller = sandbox.root.path().join("caller");
    std::fs::create_dir_all(&caller).unwrap();
    assert!(Command::new("git")
        .arg("-C")
        .arg(&caller)
        .args(["init", "-q"])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&caller)
        .args(["config", "core.sshCommand", "missing-caller-ssh"])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&caller)
        .args(["config", "ssh.variant", "plink"])
        .status()
        .unwrap()
        .success());
    let output = sandbox
        .command()
        .current_dir(&caller)
        .env_remove("GIT_SSH_VARIANT")
        .args([
            "add",
            SOURCE,
            "--global",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The first cached clone must honor destination-based conditional includes.
    let included = sandbox.root.path().join("ssh-config");
    let configured = format!("'{}' -i 'conditional key'", sandbox.ssh.display());
    for (file, key, value) in [
        (included.clone(), "core.sshCommand".to_string(), configured),
        (
            sandbox.root.path().join("home/.gitconfig"),
            format!(
                "includeIf.gitdir:{}/**.path",
                sandbox.root.path().join("cache/sources").display()
            ),
            included.to_str().unwrap().to_string(),
        ),
    ] {
        assert!(Command::new("git")
            .arg("config")
            .arg("--file")
            .arg(file)
            .arg(key)
            .arg(value)
            .status()
            .unwrap()
            .success());
    }
    let output = sandbox
        .command()
        .current_dir(&caller)
        .env("GIT_SSH", "missing-env-ssh")
        .env_remove("GIT_SSH_VARIANT")
        .args([
            "update", "review", "--global", "--action", "apply", "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["failed"], serde_json::json!([]));
    assert!(std::fs::read_to_string(sandbox.root.path().join("ssh.log"))
        .unwrap()
        .contains("conditional key"));
    let checkout = std::fs::read_dir(sandbox.root.path().join("cache/sources/repositories"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join(".git").is_dir())
        .unwrap();
    let configured = format!("'{}' -i 'checkout key'", sandbox.ssh.display());
    let output = Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["config", "core.sshCommand", &configured])
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = sandbox
        .command()
        .current_dir(caller)
        .env("GIT_SSH", "missing-env-ssh")
        .env_remove("GIT_SSH_VARIANT")
        .args([
            "update", "review", "--global", "--action", "apply", "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["failed"], serde_json::json!([]));
    assert!(std::fs::read_to_string(sandbox.root.path().join("ssh.log"))
        .unwrap()
        .contains("checkout key"));
}

#[cfg(feature = "marketplace")]
#[test]
fn scp_marketplace_and_named_and_pinned_plugins_use_the_shared_git_runner() {
    let sandbox = Sandbox::new();
    sandbox.write(
        ".claude-plugin/marketplace.json",
        r#"{"name":"team","plugins":[{"name":"relative","source":"./plugin"}]}"#,
    );
    sandbox.write(
        "plugin/.claude-plugin/plugin.json",
        r#"{"name":"fixture","version":"1.0.0"}"#,
    );
    sandbox.write(
        "plugin/skills/review/SKILL.md",
        "---\nname: review\ndescription: Review\n---\npinned\n",
    );
    let pin = sandbox.commit();
    sandbox.git(&["tag", "release"]);
    sandbox.write(".claude-plugin/marketplace.json", &serde_json::json!({
        "name": "team", "plugins": [
            {"name": "relative", "source": "./plugin"},
            {"name": "named", "source": {"source": "git", "url": SOURCE, "path": "plugin", "ref": "release"}},
            {"name": "pinned", "source": {"source": "git", "url": SOURCE, "path": "plugin", "sha": pin}}
        ]
    }).to_string());
    sandbox.write(
        "plugin/skills/review/SKILL.md",
        "---\nname: review\ndescription: Review\n---\nlatest\n",
    );
    sandbox.commit();
    sandbox.success(&["marketplace", "add", SOURCE, "--global"]);
    for plugin in ["relative@team", "named@team", "pinned@team"] {
        sandbox.success(&["plugin", "add", plugin, "--global", "--no-enable"]);
    }
    let installations = find_file(&sandbox.root.path().join("data"), "installations.json").unwrap();
    let state: Value = serde_json::from_slice(&std::fs::read(installations).unwrap()).unwrap();
    assert_eq!(
        state["plugins"]["pinned@team"]["definition"]["import"]["package_revision"],
        pin
    );
    assert_eq!(
        state["plugins"]["named@team"]["definition"]["import"]["package_revision"],
        pin
    );
    assert_eq!(state["plugins"]["pinned@team"]["source"]["url"], SOURCE);
    sandbox.success(&["marketplace", "update", "team", "--global"]);
    sandbox.success(&["plugin", "update", "pinned@team", "--global"]);
}

#[cfg(feature = "marketplace")]
fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()? {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().unwrap() == name {
            return Some(path);
        }
    }
    None
}

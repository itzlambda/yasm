use std::ffi::OsStr;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::process::Command;

#[cfg(unix)]
use expectrl::{process::unix::WaitStatus, Eof, Expect, Session};
use serde_json::Value;
use tempfile::{tempdir, TempDir};

struct TestCommand {
    command: Command,
    _sandbox: TempDir,
}

impl Deref for TestCommand {
    type Target = Command;

    fn deref(&self) -> &Self::Target {
        &self.command
    }
}

impl DerefMut for TestCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.command
    }
}

fn yasm() -> TestCommand {
    let sandbox = tempdir().unwrap();
    // Match the child's current_dir(), which resolves aliases such as macOS /var.
    let root = sandbox.path().canonicalize().unwrap();
    let home = root.join("home");
    let working_dir = root.join("workspace");
    let temp_dir = root.join("tmp");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&working_dir).unwrap();
    std::fs::create_dir_all(&temp_dir).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_yasm"));
    command
        .current_dir(&working_dir)
        .env("HOME", &home)
        .env("TMPDIR", &temp_dir)
        .env("YASM_DATA_DIR", root.join("data"))
        .env("YASM_CACHE_DIR", root.join("cache"))
        .env("YASM_CONFIG_DIR", root.join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", root.join("agents"));

    TestCommand {
        command,
        _sandbox: sandbox,
    }
}

fn yasm_with_roots(data: &Path, agents: &Path) -> TestCommand {
    let mut command = yasm();
    command
        .env("HOME", agents)
        .env("YASM_DATA_DIR", data)
        .env("YASM_CACHE_DIR", data.join("cache"))
        .env("YASM_CONFIG_DIR", data.join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agents);
    command
}

#[test]
fn rejected_source_diagnostics_do_not_echo_credentials() {
    for source in [
        "https://user:review-secret@github.com/team/repo",
        "https://github.com/team/repo?token=review-secret",
        "git:review-secret@host:repo",
    ] {
        let output = yasm()
            .args([
                "add",
                source,
                "--global",
                "--no-enable",
                "--action",
                "apply",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let diagnostics = format!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!diagnostics.contains("review-secret"));
        assert!(diagnostics.contains("invalid source:"));
    }
}

#[test]
fn self_bundle_installs_offline_and_reports_provenance() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();

    let add = yasm_with_roots(data.path(), agents.path())
        .args([
            "add", "self", "--global", "--agent", "claude", "--action", "apply",
        ])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&add.stdout),
        "installed yasm for claude\n"
    );
    assert_eq!(
        std::fs::read_to_string(data.path().join("skills/yasm/SKILL.md")).unwrap(),
        "---\nname: yasm\ndescription: TODO\n---\n\nTODO\n"
    );
    assert!(
        std::fs::symlink_metadata(agents.path().join(".claude/skills/yasm"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!agents.path().join(".agents/skills/yasm").exists());

    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert_eq!(lock["version"], 3);
    assert_eq!(lock["skills"]["yasm"]["source"]["kind"], "bundled");
    assert_eq!(lock["skills"]["yasm"]["source"]["path"], "self");
    assert_eq!(
        lock["bundles"]["self"]["members"],
        serde_json::json!(["yasm"])
    );
    assert_eq!(
        lock["bundles"]["self"]["enabled"],
        serde_json::json!(["claude"])
    );
    assert_eq!(
        lock["bundles"]["self"]["member_enabled"]["yasm"],
        serde_json::json!(["claude"])
    );
    assert_eq!(
        lock["bundles"]["self"]["digest"].as_str().unwrap().len(),
        64
    );

    let info = yasm_with_roots(data.path(), agents.path())
        .args(["info", "self", "--global"])
        .output()
        .unwrap();
    assert!(info.status.success());
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(stdout.contains("Name: Yasm bundled skills"));
    assert!(stdout.contains("Bundle content: current"));
    assert!(stdout.contains("yasm: acquired"));

    let list = yasm_with_roots(data.path(), agents.path())
        .args(["list", "--global"])
        .output()
        .unwrap();
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("Yasm bundled skills"));
}

#[test]
fn self_bundle_update_protects_edits_and_preserves_agent_selection() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["add", "self", "--global", "--agent", "claude", "--action", "apply",])
        .status()
        .unwrap()
        .success());
    let skill = data.path().join("skills/yasm/SKILL.md");
    std::fs::write(&skill, "local edit\n").unwrap();

    let guarded = yasm_with_roots(data.path(), agents.path())
        .args(["update", "self", "--global"])
        .output()
        .unwrap();
    assert!(!guarded.status.success());
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), "local edit\n");
    assert!(String::from_utf8_lossy(&guarded.stderr).contains("--action <review|apply|skip>"));

    let update = yasm_with_roots(data.path(), agents.path())
        .args(["update", "self", "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(update.status.success());
    assert!(String::from_utf8_lossy(&update.stdout).contains("updated yasm"));
    assert!(std::fs::read_to_string(&skill).unwrap().ends_with("TODO\n"));
    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["yasm"]["enabled"],
        serde_json::json!(["claude"])
    );
    assert_eq!(
        lock["bundles"]["self"]["enabled"],
        serde_json::json!(["claude"])
    );
}

#[test]
fn self_bundle_add_refuses_to_overwrite_an_unlocked_store_directory() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let store = data.path().join("skills/yasm");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("private.txt"), "keep me\n").unwrap();

    let add = yasm_with_roots(data.path(), agents.path())
        .args([
            "add",
            "self",
            "--global",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();

    assert!(!add.status.success());
    assert!(String::from_utf8_lossy(&add.stderr).contains("without a lock record"));
    assert_eq!(
        std::fs::read_to_string(store.join("private.txt")).unwrap(),
        "keep me\n"
    );
    assert!(!data.path().join("yasm.lock").exists());
}

#[test]
fn commands_reject_old_lockfiles_without_changing_state() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let lock_path = data.path().join("yasm.lock");
    let old_lock = r#"{"version":2,"skills":{}}"#;
    std::fs::write(&lock_path, old_lock).unwrap();
    for args in [
        vec!["info", "self", "--global"],
        vec!["remove", "self", "--global", "--all"],
        vec!["list", "--global"],
    ] {
        let result = yasm_with_roots(data.path(), agents.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("unsupported lockfile version 2"));
        assert_eq!(std::fs::read_to_string(&lock_path).unwrap(), old_lock);
    }
}

#[test]
fn self_bundle_exclusions_survive_members_absent_from_the_running_executable() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    assert!(yasm_with_roots(data.path(), agents.path())
        .args([
            "add",
            "self",
            "--global",
            "--no-enable",
            "--action",
            "apply",
        ])
        .status()
        .unwrap()
        .success());

    let lock_path = data.path().join("yasm.lock");
    let mut lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    lock["bundles"]["self"]["members"] = serde_json::json!(["retired", "yasm"]);
    lock["bundles"]["self"]["excluded"] = serde_json::json!(["retired"]);
    lock["bundles"]["self"]["member_enabled"] = serde_json::json!({
        "retired": ["claude"]
    });
    lock["bundles"]["self"]["digest"] = serde_json::json!("older-bundle");
    std::fs::write(&lock_path, serde_json::to_vec_pretty(&lock).unwrap()).unwrap();

    let update = yasm_with_roots(data.path(), agents.path())
        .args(["update", "self", "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(
        lock["bundles"]["self"]["excluded"],
        serde_json::json!(["retired"])
    );
    assert_eq!(
        lock["bundles"]["self"]["members"],
        serde_json::json!(["yasm"])
    );
    assert_eq!(
        lock["bundles"]["self"]["member_enabled"]["retired"],
        serde_json::json!(["claude"])
    );

    let add = yasm_with_roots(data.path(), agents.path())
        .args([
            "add",
            "self",
            "--global",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(add.status.success());
    let lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(
        lock["bundles"]["self"]["excluded"],
        serde_json::json!(["retired"])
    );
}

#[test]
fn individual_bundle_update_refreshes_bundle_receipt() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    assert!(yasm_with_roots(data.path(), agents.path())
        .args([
            "add",
            "self",
            "--global",
            "--no-enable",
            "--action",
            "apply",
        ])
        .status()
        .unwrap()
        .success());

    let lock_path = data.path().join("yasm.lock");
    let mut lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    let expected_release = lock["bundles"]["self"]["release"].clone();
    let expected_digest = lock["bundles"]["self"]["digest"].clone();
    lock["bundles"]["self"]["release"] = serde_json::json!("older-release");
    lock["bundles"]["self"]["digest"] = serde_json::json!("older-bundle");
    std::fs::write(&lock_path, serde_json::to_vec_pretty(&lock).unwrap()).unwrap();
    std::fs::write(data.path().join("skills/yasm/SKILL.md"), "local edit\n").unwrap();

    let update = yasm_with_roots(data.path(), agents.path())
        .args(["update", "yasm", "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(lock["bundles"]["self"]["release"], expected_release);
    assert_eq!(lock["bundles"]["self"]["digest"], expected_digest);

    let status = yasm_with_roots(data.path(), agents.path())
        .args(["status", "--global"])
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(!String::from_utf8_lossy(&status.stdout).contains("bundled content differs"));
}

#[test]
fn individual_bundle_update_keeps_receipt_stale_until_membership_converges() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    assert!(yasm_with_roots(data.path(), agents.path())
        .args([
            "add",
            "self",
            "--global",
            "--no-enable",
            "--action",
            "apply"
        ])
        .status()
        .unwrap()
        .success());

    let lock_path = data.path().join("yasm.lock");
    let mut lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    let mut retired = lock["skills"]["yasm"].clone();
    retired["name"] = serde_json::json!("retired");
    retired["skill_path"] = serde_json::json!("retired/SKILL.md");
    lock["skills"]["retired"] = retired;
    lock["bundles"]["self"]["members"] = serde_json::json!(["retired", "yasm"]);
    lock["bundles"]["self"]["release"] = serde_json::json!("older-release");
    lock["bundles"]["self"]["digest"] = serde_json::json!("older-bundle");
    std::fs::write(&lock_path, serde_json::to_vec_pretty(&lock).unwrap()).unwrap();
    std::fs::write(data.path().join("skills/yasm/SKILL.md"), "local edit\n").unwrap();

    let update = yasm_with_roots(data.path(), agents.path())
        .args(["update", "yasm", "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert!(String::from_utf8_lossy(&update.stdout).contains("updated yasm"));
    let actual: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(actual["bundles"]["self"], lock["bundles"]["self"]);
    assert_eq!(actual["skills"]["retired"], lock["skills"]["retired"]);
    assert_eq!(
        std::fs::read_to_string(data.path().join("skills/yasm/SKILL.md")).unwrap(),
        "---\nname: yasm\ndescription: TODO\n---\n\nTODO\n"
    );
}

#[test]
fn skipped_bundle_changes_preserve_receipts_and_local_contents() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["add", "self", "--global", "--agent", "claude", "--action", "apply"])
        .status()
        .unwrap()
        .success());

    let lock_path = data.path().join("yasm.lock");
    let mut lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    lock["bundles"]["self"]["release"] = serde_json::json!("older-release");
    lock["bundles"]["self"]["digest"] = serde_json::json!("older-bundle");
    std::fs::write(&lock_path, serde_json::to_vec_pretty(&lock).unwrap()).unwrap();
    let skill_path = data.path().join("skills/yasm/SKILL.md");
    std::fs::write(&skill_path, "local edit\n").unwrap();

    for args in [
        vec!["update", "self", "--global", "--action", "skip"],
        vec!["update", "yasm", "--global", "--action", "skip"],
        vec!["add", "self", "--global", "--no-enable", "--action", "skip"],
    ] {
        let output = yasm_with_roots(data.path(), agents.path())
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
        assert_eq!(actual, lock, "{args:?} changed the lockfile");
        assert_eq!(
            std::fs::read_to_string(&skill_path).unwrap(),
            "local edit\n"
        );
    }
}

#[test]
fn self_bundle_update_restores_missing_members_with_saved_agent_selection() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["add", "self", "--global", "--agent", "claude", "--action", "apply"])
        .status()
        .unwrap()
        .success());

    let lock_path = data.path().join("yasm.lock");
    let mut lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    let expected_digest = lock["bundles"]["self"]["digest"].clone();
    lock["skills"].as_object_mut().unwrap().remove("yasm");
    lock["bundles"]["self"]["enabled"] = serde_json::json!(["universal"]);
    lock["bundles"]["self"]["digest"] = serde_json::json!("older-bundle");
    std::fs::write(&lock_path, serde_json::to_vec_pretty(&lock).unwrap()).unwrap();
    std::fs::remove_dir_all(data.path().join("skills/yasm")).unwrap();
    std::fs::remove_file(agents.path().join(".claude/skills/yasm")).unwrap();

    let skipped = yasm_with_roots(data.path(), agents.path())
        .args(["update", "self", "--global", "--action", "skip"])
        .output()
        .unwrap();
    assert!(skipped.status.success());
    let actual: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(actual, lock);
    assert!(!data.path().join("skills/yasm").exists());

    let update = yasm_with_roots(data.path(), agents.path())
        .args(["update", "self", "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert!(String::from_utf8_lossy(&update.stdout).contains("added bundled skill yasm"));
    let actual: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(actual["bundles"]["self"]["digest"], expected_digest);
    assert_eq!(
        actual["bundles"]["self"]["enabled"],
        serde_json::json!(["universal"])
    );
    assert_eq!(
        actual["bundles"]["self"]["member_enabled"]["yasm"],
        serde_json::json!(["claude"])
    );
    assert_eq!(
        actual["skills"]["yasm"]["enabled"],
        serde_json::json!(["claude"])
    );
    assert!(agents.path().join(".claude/skills/yasm/SKILL.md").exists());
    assert!(!agents.path().join(".agents/skills/yasm").exists());
}

#[test]
fn individual_bundle_removal_stays_excluded_until_readded() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["add", "self", "--global", "--action", "apply"])
        .status()
        .unwrap()
        .success());

    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["remove", "yasm", "--global", "--all"])
        .status()
        .unwrap()
        .success());
    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["update", "self", "--global", "--action", "apply"])
        .status()
        .unwrap()
        .success());
    assert!(!data.path().join("skills/yasm").exists());
    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["bundles"]["self"]["excluded"],
        serde_json::json!(["yasm"])
    );

    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["add", "self", "--global", "--agent", "claude", "--action", "apply"])
        .status()
        .unwrap()
        .success());
    assert!(data.path().join("skills/yasm/SKILL.md").exists());
    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert!(lock["bundles"]["self"].get("excluded").is_none());
    assert_eq!(
        lock["skills"]["yasm"]["enabled"],
        serde_json::json!(["claude"])
    );

    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["remove", "self", "--global", "--all"])
        .status()
        .unwrap()
        .success());
    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert!(lock.get("bundles").is_none());
    assert!(lock["skills"].as_object().unwrap().is_empty());
}

#[test]
fn self_target_reservation_keeps_dot_self_available_as_a_local_source() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let sources = tempdir().unwrap();
    write_skill(
        sources.path(),
        "self/example",
        "example",
        "Example skill",
        "body",
    );

    let local = yasm_with_roots(data.path(), agents.path())
        .current_dir(sources.path())
        .args(["add", "./self", "--no-enable", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        local.status.success(),
        "local add failed: {}",
        String::from_utf8_lossy(&local.stderr)
    );
    assert!(data.path().join("skills/example/SKILL.md").exists());

    let reserved_source = tempdir().unwrap();
    write_skill(
        reserved_source.path(),
        "skill",
        "self",
        "Reserved skill",
        "body",
    );
    let reserved = yasm_with_roots(data.path(), agents.path())
        .arg("add")
        .arg(reserved_source.path())
        .args(["--no-enable", "--action", "apply"])
        .output()
        .unwrap();
    assert!(!reserved.status.success());
    assert!(String::from_utf8_lossy(&reserved.stderr).contains("skill ID `self` is reserved"));
}

#[test]
fn self_target_controls_visibility_and_removal_for_every_member() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    assert!(yasm_with_roots(data.path(), agents.path())
        .args([
            "add",
            "self",
            "--global",
            "--no-enable",
            "--action",
            "apply",
        ])
        .status()
        .unwrap()
        .success());

    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["enable", "self", "--global", "--agent", "claude"])
        .status()
        .unwrap()
        .success());
    let link = agents.path().join(".claude/skills/yasm");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());

    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["disable", "self", "--global", "--agent", "claude"])
        .status()
        .unwrap()
        .success());
    assert!(!link.exists());
    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["remove", "self", "--global"])
        .status()
        .unwrap()
        .success());
    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert!(lock.get("bundles").is_none());
    assert!(lock["skills"].as_object().unwrap().is_empty());
}

#[test]
fn self_update_retires_members_absent_from_the_running_bundle() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    assert!(yasm_with_roots(data.path(), agents.path())
        .args([
            "add",
            "self",
            "--global",
            "--no-enable",
            "--action",
            "apply",
        ])
        .status()
        .unwrap()
        .success());

    let lock_path = data.path().join("yasm.lock");
    let mut lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    let mut retired = lock["skills"]["yasm"].clone();
    retired["name"] = serde_json::json!("retired");
    retired["skill_path"] = serde_json::json!("retired/SKILL.md");
    lock["skills"]["retired"] = retired;
    lock["bundles"]["self"]["members"] = serde_json::json!(["retired", "yasm"]);
    std::fs::write(&lock_path, serde_json::to_vec_pretty(&lock).unwrap()).unwrap();
    let retired_dir = data.path().join("skills/retired");
    std::fs::create_dir(&retired_dir).unwrap();
    std::fs::copy(
        data.path().join("skills/yasm/SKILL.md"),
        retired_dir.join("SKILL.md"),
    )
    .unwrap();

    let update = yasm_with_roots(data.path(), agents.path())
        .args(["update", "self", "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert!(String::from_utf8_lossy(&update.stdout).contains("retired bundled skill retired"));
    assert!(!retired_dir.exists());
    let lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert!(lock["skills"].get("retired").is_none());
    assert_eq!(
        lock["bundles"]["self"]["members"],
        serde_json::json!(["yasm"])
    );
}

#[test]
fn self_bundle_uses_the_existing_project_scope() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let project = tempdir().unwrap();
    let init = yasm_with_roots(data.path(), agents.path())
        .current_dir(project.path())
        .args(["init", "--no-migrate"])
        .output()
        .unwrap();
    assert!(init.status.success());

    let add = yasm_with_roots(data.path(), agents.path())
        .current_dir(project.path())
        .args(["add", "self", "--agent", "universal", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "project add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(project.path().join(".yasm/skills/yasm/SKILL.md").exists());
    assert!(
        std::fs::symlink_metadata(project.path().join(".agents/skills/yasm"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!data.path().join("skills/yasm").exists());
}

#[test]
fn commands_are_sandboxed_by_default() {
    let first = yasm();
    let second = yasm();
    let first_root = first
        .get_current_dir()
        .unwrap()
        .parent()
        .expect("sandbox working directory should have a parent");
    let second_root = second
        .get_current_dir()
        .unwrap()
        .parent()
        .expect("sandbox working directory should have a parent");

    assert_ne!(first_root, second_root);
    assert_eq!(first_root, first_root.canonicalize().unwrap());
    for name in [
        "HOME",
        "TMPDIR",
        "YASM_DATA_DIR",
        "YASM_CACHE_DIR",
        "YASM_CONFIG_DIR",
        "YASM_AGENT_SKILLS_ROOT",
    ] {
        let value = first
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(name))
            .and_then(|(_, value)| value)
            .unwrap_or_else(|| panic!("{name} should be configured"));
        assert!(Path::new(value).starts_with(first_root));
    }
}

#[test]
fn unchanged_update_outside_git_prints_only_the_result() {
    let output = yasm().arg("update").output().unwrap();

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "no changes\n");
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn init_tip_is_limited_to_interactive_unscoped_git_work_trees() {
    let non_git = tempdir().unwrap();
    let mut command = yasm();
    command.current_dir(non_git.path()).arg("status");
    assert!(!tty_output(command).contains("yasm init"));

    let repository = tempdir().unwrap();
    git(repository.path(), &["init"]);

    let mut root_command = yasm();
    root_command.current_dir(repository.path()).arg("status");
    let output = tty_output(root_command);
    assert!(
        output.contains("Tip: run `yasm init` to manage skills for this repository."),
        "{output:?}"
    );
    assert!(!output.contains("Scope:"));
    assert!(!output.contains("Store:"));

    let nested = repository.path().join("src/nested");
    std::fs::create_dir_all(&nested).unwrap();
    let mut nested_command = yasm();
    nested_command.current_dir(&nested).arg("status");
    assert!(tty_output(nested_command).contains("yasm init"));

    let mut explicit_global = yasm();
    explicit_global
        .current_dir(repository.path())
        .args(["status", "--global"]);
    assert!(!tty_output(explicit_global).contains("yasm init"));

    let redirected = yasm()
        .current_dir(repository.path())
        .arg("status")
        .output()
        .unwrap();
    assert!(redirected.status.success());
    assert!(!String::from_utf8_lossy(&redirected.stdout).contains("yasm init"));

    std::fs::create_dir(repository.path().join(".yasm")).unwrap();
    let mut project_command = yasm();
    project_command.current_dir(repository.path()).arg("status");
    assert!(!tty_output(project_command).contains("yasm init"));

    std::fs::remove_dir(repository.path().join(".yasm")).unwrap();
    git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    git(repository.path(), &["config", "user.name", "Test"]);
    std::fs::write(repository.path().join("README.md"), "test").unwrap();
    git(repository.path(), &["add", "."]);
    git(repository.path(), &["commit", "-m", "initial"]);
    let worktree_parent = tempdir().unwrap();
    let worktree = worktree_parent.path().join("linked");
    git(
        repository.path(),
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            worktree.to_str().unwrap(),
        ],
    );
    assert!(worktree.join(".git").is_file());
    let mut worktree_command = yasm();
    worktree_command.current_dir(&worktree).arg("status");
    assert!(tty_output(worktree_command).contains("yasm init"));
}

fn git(root: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        // Fixture commits must not depend on the caller's identity or signing setup.
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

fn redirect_github(command: &mut Command, remote: &std::path::Path) {
    command
        .env("GIT_CONFIG_COUNT", "1")
        .env(
            "GIT_CONFIG_KEY_0",
            format!("url.file://{}.insteadOf", remote.display()),
        )
        .env("GIT_CONFIG_VALUE_0", "https://github.com/owner/repo.git");
}

fn write_skill(root: &std::path::Path, path: &str, name: &str, description: &str, body: &str) {
    let skill_dir = root.join(path);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
}

fn prepare_migration(command: &TestCommand) -> (std::path::PathBuf, Vec<u8>) {
    let project = command.get_current_dir().unwrap().to_path_buf();
    std::fs::create_dir(project.join(".yasm")).unwrap();
    let initial_lock = b"{\n  \"version\": 3,\n  \"skills\": {}\n}\n".to_vec();
    std::fs::write(project.join(".yasm/yasm.lock"), &initial_lock).unwrap();
    write_skill(
        &project,
        ".agents/skills/demo",
        "demo",
        "Demo skill",
        "original body",
    );
    (project, initial_lock)
}

fn assert_migration_applied(project: &Path, initial_lock: &[u8]) {
    assert_eq!(
        std::fs::read_to_string(project.join(".yasm/skills/demo/SKILL.md")).unwrap(),
        "---\nname: demo\ndescription: Demo skill\n---\noriginal body\n"
    );
    assert!(
        std::fs::symlink_metadata(project.join(".agents/skills/demo"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let current_lock = std::fs::read(project.join(".yasm/yasm.lock")).unwrap();
    assert_ne!(current_lock, initial_lock);
    let lock: Value = serde_json::from_slice(&current_lock).unwrap();
    assert_eq!(lock["skills"]["demo"]["source"]["kind"], "owned");
    assert_eq!(
        lock["skills"]["demo"]["enabled"],
        serde_json::json!(["universal"])
    );
}

fn assert_migration_not_applied(project: &Path, initial_lock: &[u8]) {
    assert!(!project.join(".yasm/skills/demo").exists());
    let source = project.join(".agents/skills/demo");
    assert!(!std::fs::symlink_metadata(&source)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_to_string(source.join("SKILL.md")).unwrap(),
        "---\nname: demo\ndescription: Demo skill\n---\noriginal body\n"
    );
    assert_eq!(
        std::fs::read(project.join(".yasm/yasm.lock")).unwrap(),
        initial_lock
    );
}

#[cfg(unix)]
fn tty_output(command: TestCommand) -> String {
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();
    let output = session.expect(Eof).unwrap();
    let stdout = String::from_utf8_lossy(output.as_bytes()).into_owned();
    let status = session.get_process().wait().unwrap();
    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    stdout
}

fn list_json_value(yasm_home: &std::path::Path, agent_home: &std::path::Path) -> Value {
    let list_json = yasm()
        .env("YASM_DATA_DIR", yasm_home)
        .env("YASM_CACHE_DIR", yasm_home.join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home)
        .arg("list")
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        list_json.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&list_json.stderr)
    );
    let json: Value = serde_json::from_slice(&list_json.stdout).unwrap();
    json["global"].clone()
}

fn update_failure_fixture() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/broken-a",
        "broken-a",
        "Broken skill A",
        "old broken A",
    );
    write_skill(
        source.path(),
        "skills/broken-b",
        "broken-b",
        "Broken skill B",
        "old broken B",
    );
    write_skill(
        source.path(),
        "skills/working",
        "working",
        "Working skill",
        "old working",
    );

    for skill in ["broken-a", "broken-b", "working"] {
        let add = yasm()
            .env("YASM_DATA_DIR", yasm_home.path())
            .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
            .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
            .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
            .arg("add")
            .arg("--action")
            .arg("apply")
            .arg("--no-enable")
            .arg(source.path())
            .arg("--skill")
            .arg(skill)
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }

    std::fs::remove_dir_all(source.path().join("skills/broken-a")).unwrap();
    std::fs::remove_dir_all(source.path().join("skills/broken-b")).unwrap();
    write_skill(
        source.path(),
        "skills/working",
        "working",
        "Working skill",
        "new working",
    );
    (yasm_home, agent_home, source)
}

#[test]
fn help_and_version_are_discoverable() {
    let help = yasm().arg("--help").output().unwrap();
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("Initialize projects, adopt existing skills"));
    assert!(stdout.contains("Examples:"));
    assert!(stdout.contains("Support: https://github.com/itzlambda/yasm"));
    assert!(stdout.contains("self-upgrade"));
    assert!(stdout.contains("--version"));
    assert!(!stdout.contains("-V, --version"));
    assert!(!stdout.contains("-v, --version"));

    let add_help = yasm().arg("add").arg("--help").output().unwrap();
    assert!(add_help.status.success());
    let add_stdout = String::from_utf8_lossy(&add_help.stdout);
    assert!(add_stdout.contains("Acquire skills into the yasm store"));
    assert!(add_stdout.contains("--agent <AGENT>"));
    assert!(add_stdout.contains("--no-enable"));
    assert!(add_stdout.contains("possible values: universal, claude"));

    let version = yasm().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("yasm"));

    let short_version = yasm().arg("-V").output().unwrap();
    assert!(!short_version.status.success());
}

#[test]
fn self_upgrade_requires_confirmation_without_a_terminal() {
    let help = yasm().args(["self-upgrade", "--help"]).output().unwrap();
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("Upgrade the yasm binary from GitHub releases"));
    assert!(stdout.contains("--yes"));

    let missing = yasm().arg("self-upgrade").output().unwrap();
    assert!(!missing.status.success());
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(stderr.contains("missing confirmation"));
    assert!(stderr.contains("--yes"));
}

#[test]
fn invalid_agent_uses_clap_value_error() {
    let result = yasm()
        .arg("list")
        .arg("--agent")
        .arg("codex")
        .output()
        .unwrap();

    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("invalid value 'codex'"));
    assert!(stderr.contains("universal"));
    assert!(stderr.contains("claude"));
}

#[test]
fn add_rejects_non_main_github_tree_refs_during_argument_parsing() {
    let result = yasm()
        .arg("add")
        .arg("https://github.com/anthropics/claude-code/tree/develop/plugins/frontend-design")
        .output()
        .unwrap();

    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("only `main` tree URLs are supported"));
    assert!(!stderr.contains("Select skills to install"));
}

#[test]
fn github_tree_url_scopes_add_and_later_updates_to_its_directory() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let remote = tempdir().unwrap();
    git(remote.path(), &["init", "-b", "main"]);
    git(remote.path(), &["config", "user.email", "test@example.com"]);
    git(remote.path(), &["config", "user.name", "Test"]);
    write_skill(
        remote.path(),
        "skills/selected",
        "selected",
        "Selected skill",
        "old",
    );
    write_skill(
        remote.path(),
        "skills/sibling",
        "sibling",
        "Sibling skill",
        "body",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "initial"]);

    let mut add = yasm();
    redirect_github(&mut add, remote.path());
    let add = add
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg("https://github.com/owner/repo/tree/main/skills/selected")
        .output()
        .unwrap();

    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(yasm_home.path().join("skills/selected").exists());
    assert!(!yasm_home.path().join("skills/sibling").exists());
    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(json["skills"]["selected"]["source"]["ref"], "main");
    assert!(json["skills"]["selected"]["source"]
        .get("subpath")
        .is_none());
    assert_eq!(
        json["skills"]["selected"]["skill_path"],
        "skills/selected/SKILL.md"
    );
    let list = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("https://github.com/owner/repo"));

    write_skill(
        remote.path(),
        "skills/selected",
        "selected",
        "Selected skill",
        "new",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "update"]);

    let mut update = yasm();
    redirect_github(&mut update, remote.path());
    let update = update
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("selected")
        .arg("--action")
        .arg("apply")
        .output()
        .unwrap();

    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let installed =
        std::fs::read_to_string(yasm_home.path().join("skills/selected/SKILL.md")).unwrap();
    assert!(installed.contains("new"));
}

#[test]
fn github_tree_url_reports_when_its_directory_has_no_skill() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let remote = tempdir().unwrap();
    git(remote.path(), &["init", "-b", "main"]);
    git(remote.path(), &["config", "user.email", "test@example.com"]);
    git(remote.path(), &["config", "user.name", "Test"]);
    std::fs::create_dir(remote.path().join("docs")).unwrap();
    std::fs::write(remote.path().join("docs/README.md"), "No skill here").unwrap();
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "docs"]);

    let mut add = yasm();
    redirect_github(&mut add, remote.path());
    let add = add
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg("https://github.com/owner/repo/tree/main/docs")
        .output()
        .unwrap();

    assert!(!add.status.success());
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(stderr.contains("no valid skills found in Git directory `docs`"));
}

#[test]
fn add_skips_broken_sibling_and_installs_selected_valid_skill() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "agent-compatibility/skills/check-agent-compatibility",
        "check-agent-compatibility",
        "Run the full repository compatibility pass: scanner score, startup path, validation loop, and docs reliability.",
        "body",
    );
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "Design UI",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg(source.path())
        .arg("--skill")
        .arg("frontend-design")
        .output()
        .unwrap();

    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(stderr.contains("warning: skipped skill"));
    assert!(stderr.contains("agent-compatibility/skills/check-agent-compatibility/SKILL.md"));
    assert!(stderr.contains("parse error"));
    assert!(!stderr.contains("mapping values are not allowed"));
    assert!(yasm_home.path().join("skills/frontend-design").exists());
    assert!(!yasm_home
        .path()
        .join("skills/check-agent-compatibility")
        .exists());
}

#[test]
fn add_requested_broken_skill_fails_after_reporting_it() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "agent-compatibility/skills/check-agent-compatibility",
        "check-agent-compatibility",
        "Run the full repository compatibility pass: scanner score, startup path, validation loop, and docs reliability.",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg(source.path())
        .arg("--skill")
        .arg("check-agent-compatibility")
        .output()
        .unwrap();

    assert!(!add.status.success());
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(stderr.contains("warning: skipped skill"));
    assert!(stderr.contains("parse error"));
    assert!(!stderr.contains("mapping values are not allowed"));
    assert!(stderr.contains("skill `check-agent-compatibility` was not found"));
    assert!(stderr.contains("available skills: none"));
    assert!(!yasm_home
        .path()
        .join("skills/check-agent-compatibility")
        .exists());
}

#[test]
fn add_list_and_remove_local_skill() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = agent_home.path().join("source");
    write_skill(
        &source,
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("HOME", agent_home.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(&source)
        .arg("--skill")
        .arg("frontend-design")
        .arg("--agent")
        .arg("universal")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&add.stdout),
        "installed frontend-design for universal, claude\n"
    );

    let store_skill = yasm_home.path().join("skills/frontend-design/SKILL.md");
    assert!(store_skill.exists());
    let universal_link = agent_home.path().join(".agents/skills/frontend-design");
    assert!(std::fs::symlink_metadata(&universal_link)
        .unwrap()
        .file_type()
        .is_symlink());
    let claude_link = agent_home.path().join(".claude/skills/frontend-design");
    assert!(std::fs::symlink_metadata(&claude_link)
        .unwrap()
        .file_type()
        .is_symlink());

    let list = yasm()
        .env("HOME", agent_home.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(
        list.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(list_stdout.contains("Global Skills (1)"));
    assert!(list_stdout.contains("│ Skill            │ Source"));
    assert!(!list_stdout.contains("Installed"));
    assert!(list_stdout.contains("├──────────────────┼"));
    assert!(list_stdout.contains("│ frontend-design  │ ~/source"));
    assert!(!list_stdout.contains("~/source/skills/frontend-design/SKILL.md"));
    assert!(!list_stdout.contains(&yasm_home.path().display().to_string()));
    assert!(!list_stdout.contains(&agent_home.path().display().to_string()));

    let info = yasm()
        .env("HOME", agent_home.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["info", "frontend-design"])
        .output()
        .unwrap();
    assert!(info.status.success());
    let info_stdout = String::from_utf8_lossy(&info.stdout);
    assert!(info_stdout.contains("Name: frontend-design"));
    assert!(info_stdout.contains("ID: frontend-design"));
    assert!(info_stdout.contains("Scope: global"));
    assert!(info_stdout.contains("Enabled: claude, universal"));
    assert!(info_stdout.contains(&format!(
        "Installed: {}/skills/frontend-design/SKILL.md",
        yasm_home.path().display()
    )));
    assert!(info_stdout.contains("Source type: local"));
    assert!(info_stdout.contains("Source root: ~/source"));
    assert!(info_stdout.contains("Skill file: ~/source/skills/frontend-design/SKILL.md"));

    let list_json = yasm()
        .env("HOME", agent_home.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("list")
        .arg("--json")
        .output()
        .unwrap();
    assert!(list_json.status.success());
    assert!(String::from_utf8_lossy(&list_json.stdout).contains("\"skills\""));

    let disable_json = yasm()
        .env("HOME", agent_home.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("disable")
        .arg("frontend-design")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();
    assert!(
        disable_json.status.success(),
        "disable failed: {}",
        String::from_utf8_lossy(&disable_json.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&disable_json.stdout),
        "disabled frontend-design for claude\n"
    );
    assert!(!claude_link.exists());
    assert!(universal_link.exists());
    assert!(yasm_home.path().join("skills/frontend-design").exists());

    let remove = yasm()
        .env("HOME", agent_home.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("remove")
        .arg("frontend-design")
        .arg("--all")
        .output()
        .unwrap();
    assert!(
        remove.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&remove.stdout),
        "removed frontend-design\n"
    );
    assert!(!universal_link.exists());
    assert!(!claude_link.exists());
    assert!(!yasm_home.path().join("skills/frontend-design").exists());
}

#[test]
fn list_and_info_report_per_harness_skill_metadata() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let source = tempdir().unwrap();
    let skills = [
        ("automatic", "Automatic description", "", None),
        (
            "shared-manual",
            "Shared manual description",
            "disable-model-invocation: true\n",
            None,
        ),
        (
            "codex-manual",
            "Codex manual description",
            "disable-model-invocation: false\n",
            Some("policy:\n  allow_implicit_invocation: false\n"),
        ),
        (
            "all-manual",
            "All manual description",
            "disable-model-invocation: true\n",
            Some("policy:\n  allow_implicit_invocation: false\n"),
        ),
    ];
    for (name, description, extra_frontmatter, openai) in skills {
        let directory = source.path().join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {description}\n{extra_frontmatter}---\nbody\n"
            ),
        )
        .unwrap();
        if let Some(openai) = openai {
            std::fs::create_dir_all(directory.join("agents")).unwrap();
            std::fs::write(directory.join("agents/openai.yaml"), openai).unwrap();
        }

        let add = yasm()
            .env("YASM_DATA_DIR", data.path())
            .env("YASM_AGENT_SKILLS_ROOT", agents.path())
            .args(["add", "--action", "apply", "--no-enable"])
            .arg(source.path())
            .args(["--skill", name])
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }

    let list = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(list.status.success());
    assert!(list.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("Manual only"), "{stdout}");
    assert!(stdout
        .lines()
        .any(|line| line.contains("automatic") && line.contains('—')));
    assert!(stdout.lines().any(|line| {
        line.contains("shared-manual") && line.contains("Claude, Pi") && !line.contains("Codex")
    }));
    assert!(stdout.lines().any(|line| {
        line.contains("codex-manual") && line.contains("Codex") && !line.contains("Claude")
    }));
    assert!(stdout
        .lines()
        .any(|line| { line.contains("all-manual") && line.contains("Codex, Claude, Pi") }));
    assert!(!stdout.contains("Shared manual description"));

    let info = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .args(["info", "shared-manual"])
        .output()
        .unwrap();
    assert!(info.status.success());
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(stdout.contains("Description: Shared manual description"));
    assert!(stdout.contains("Codex invocation: automatic"));
    assert!(stdout.contains("Claude invocation: manual only"));
    assert!(stdout.contains("Pi invocation: manual only"));

    let json_output = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(json_output.status.success());
    let json: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    let skill = &json["global"]["skills"]["codex-manual"];
    assert_eq!(skill["name"], "codex-manual");
    assert!(skill.get("source").is_some());
    assert!(skill.get("enabled").is_some());
    assert_eq!(skill["metadata"]["name"], "codex-manual");
    assert_eq!(skill["metadata"]["description"], "Codex manual description");
    assert_eq!(
        skill["metadata"]["invocation"],
        serde_json::json!({
            "codex": "manual_only",
            "claude": "automatic",
            "pi": "automatic"
        })
    );

    let lockfile = std::fs::read_to_string(data.path().join("yasm.lock")).unwrap();
    assert!(!lockfile.contains("metadata"));
    assert!(!lockfile.contains("description"));
}

#[test]
fn malformed_installed_metadata_is_diagnostic_and_keeps_skill_visible() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "broken-metadata",
        "broken-metadata",
        "Still visible",
        "body",
    );
    let add = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .args(["add", "--action", "apply", "--no-enable"])
        .arg(source.path())
        .args(["--skill", "broken-metadata"])
        .output()
        .unwrap();
    assert!(add.status.success());

    let installed = data.path().join("skills/broken-metadata");
    std::fs::write(
        installed.join("SKILL.md"),
        "---\nname: broken-metadata\ndescription: Still visible\ndisable-model-invocation: manual\n---\n",
    )
    .unwrap();
    std::fs::create_dir_all(installed.join("agents")).unwrap();
    std::fs::write(installed.join("agents/openai.yaml"), "policy: [\n").unwrap();

    let list = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("broken-metadata"));
    assert!(stdout.contains("unknown: Codex, Claude, Pi"));
    let stderr = String::from_utf8_lossy(&list.stderr);
    assert_eq!(stderr.matches("warning:").count(), 3, "{stderr}");
    assert!(stderr.contains("disable-model-invocation"));
    assert!(stderr.contains("invalid Codex metadata YAML"));

    let json_output = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(json_output.status.success());
    let json: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    let metadata = &json["global"]["skills"]["broken-metadata"]["metadata"];
    assert_eq!(
        metadata["invocation"],
        serde_json::json!({
            "codex": "unknown",
            "claude": "unknown",
            "pi": "unknown"
        })
    );
    assert_eq!(metadata["diagnostics"].as_array().unwrap().len(), 3);
}

#[test]
fn list_uses_source_roots_and_hides_source_after_filtering_to_owned_skills() {
    let project = tempdir().unwrap();
    let home = tempdir().unwrap();
    let inside_source = home.path().join("Workspace/L/skills");
    let outside_source = tempdir().unwrap();
    write_skill(
        project.path(),
        ".agents/skills/owned-skill",
        "owned-skill",
        "Owned skill",
        "owned",
    );
    write_skill(
        &inside_source,
        "inside-skill",
        "inside-skill",
        "Inside skill",
        "inside",
    );
    write_skill(
        outside_source.path(),
        "outside-skill",
        "outside-skill",
        "Outside skill",
        "outside",
    );

    let init = yasm()
        .current_dir(project.path())
        .env("HOME", home.path())
        .args(["init", "--action", "apply"])
        .output()
        .unwrap();
    assert!(init.status.success());

    for (source, skill) in [
        (inside_source.as_path(), "inside-skill"),
        (outside_source.path(), "outside-skill"),
    ] {
        let add = yasm()
            .current_dir(project.path())
            .env("HOME", home.path())
            .args(["add", "--action", "apply", "--no-enable"])
            .arg(source)
            .args(["--skill", skill])
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }

    let list = yasm()
        .current_dir(project.path())
        .env("HOME", home.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("Project Skills (3)"));
    assert!(stdout.contains("Source"));
    assert!(stdout.contains("~/Workspace/L/skills"));
    assert!(stdout.contains(
        &outside_source
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    ));
    assert!(!stdout.contains("~/Workspace/L/skills/inside-skill/SKILL.md"));
    assert!(!stdout.contains("Installed"));
    assert!(stdout
        .lines()
        .any(|line| line.contains("owned-skill") && line.contains("│ —")));
    assert_eq!(
        stdout.lines().filter(|line| line.starts_with('├')).count(),
        1,
        "only the header separator should be rendered: {stdout}"
    );

    let filtered = yasm()
        .current_dir(project.path())
        .env("HOME", home.path())
        .args(["list", "--enabled"])
        .output()
        .unwrap();
    assert!(filtered.status.success());
    let filtered_stdout = String::from_utf8_lossy(&filtered.stdout);
    assert!(filtered_stdout.contains("Project Skills (1)"));
    assert!(filtered_stdout.contains("owned-skill"));
    assert!(!filtered_stdout.contains("inside-skill"));
    assert!(!filtered_stdout.contains("outside-skill"));
    assert!(!filtered_stdout.lines().any(|line| line.contains("Source")));

    let info = yasm()
        .current_dir(project.path())
        .env("HOME", home.path())
        .args(["info", "inside-skill"])
        .output()
        .unwrap();
    assert!(info.status.success());
    let info_stdout = String::from_utf8_lossy(&info.stdout);
    assert!(info_stdout.contains("Scope: project"));
    assert!(info_stdout.contains("Installed: .yasm/skills/inside-skill/SKILL.md"));
    assert!(info_stdout.contains("Source root: ~/Workspace/L/skills"));
    assert!(info_stdout.contains("Skill file: ~/Workspace/L/skills/inside-skill/SKILL.md"));
}

#[test]
fn git_info_preserves_repository_host_and_includes_subpath_ref_and_commit() {
    let remote = tempdir().unwrap();
    git(remote.path(), &["init", "-b", "main"]);
    git(remote.path(), &["config", "user.email", "test@example.com"]);
    git(remote.path(), &["config", "user.name", "Test"]);
    write_skill(
        remote.path(),
        "plugins/bundle/skills/git-skill",
        "git-skill",
        "Git skill",
        "body",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "add skill"]);
    let commit = Command::new("git")
        .arg("-C")
        .arg(remote.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(commit.status.success());
    let commit = String::from_utf8(commit.stdout).unwrap();
    let commit = commit.trim();

    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let mut add = yasm();
    add.env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .args([
            "add",
            "--action",
            "apply",
            "--no-enable",
            "https://github.com/owner/repo/tree/main/plugins/bundle",
            "--skill",
            "git-skill",
        ]);
    redirect_github(&mut add, remote.path());
    let add = add.output().unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let list = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(list.status.success());
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(list_stdout.contains("Global Skills (1)"));
    assert!(list_stdout.contains("https://github.com/owner/repo"));
    assert!(!list_stdout.contains("https://github.com/owner/repo.git"));
    assert!(!list_stdout.contains("/blob/"));

    let info = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .args(["info", "git-skill"])
        .output()
        .unwrap();
    assert!(info.status.success());
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(stdout.contains("Name: git-skill"));
    assert!(stdout.contains("ID: git-skill"));
    assert!(stdout.contains("Scope: global"));
    assert!(stdout.contains("Enabled: —"));
    assert!(stdout.contains("Source type: Git"));
    assert!(stdout.contains("Repository: https://github.com/owner/repo"));
    assert!(stdout.contains("Skill path: plugins/bundle/skills/git-skill/SKILL.md"));
    assert!(stdout.contains("Ref: main"));
    assert!(stdout.contains(&format!("Resolved commit: {commit}")));
}

#[test]
fn info_requires_an_id_and_unknown_ids_list_available_candidates() {
    let missing = yasm().arg("info").output().unwrap();
    assert!(!missing.status.success());
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(stderr.contains("required arguments"));
    assert!(stderr.contains("<skill-id>"));
    assert!(stderr.contains("Usage:"));

    let project = tempdir().unwrap();
    write_skill(
        project.path(),
        ".agents/skills/available-skill",
        "available-skill",
        "Available skill",
        "body",
    );
    assert!(yasm()
        .current_dir(project.path())
        .args(["init", "--action", "apply"])
        .output()
        .unwrap()
        .status
        .success());
    let unknown = yasm()
        .current_dir(project.path())
        .args(["info", "missing-skill"])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr)
        .contains("skill `missing-skill` is not acquired; acquired skills: available-skill"));
}

#[test]
fn relative_local_source_updates_from_a_different_working_directory() {
    let workspace = tempdir().unwrap();
    let source = workspace.path().join("source");
    let other_directory = workspace.path().join("elsewhere");
    std::fs::create_dir(&other_directory).unwrap();
    write_skill(
        &source,
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "old",
    );
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();

    let add = yasm()
        .current_dir(workspace.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg("./source")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["source"]["path"],
        source.canonicalize().unwrap().display().to_string()
    );

    write_skill(
        &source,
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let update = yasm()
        .current_dir(&other_directory)
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("frontend-design")
        .arg("--action")
        .arg("apply")
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let installed =
        std::fs::read_to_string(yasm_home.path().join("skills/frontend-design/SKILL.md")).unwrap();
    assert!(installed.contains("new"));
}

#[test]
fn home_relative_local_source_is_expanded_before_storage() {
    let yasm_home = tempdir().unwrap();
    let home = tempdir().unwrap();
    let source = home.path().join("source");
    write_skill(
        &source,
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("HOME", home.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg("~/source")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let json = list_json_value(yasm_home.path(), home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["source"]["path"],
        source.canonicalize().unwrap().display().to_string()
    );
}

#[test]
fn add_missing_local_source_reports_the_path() {
    let workspace = tempdir().unwrap();
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();

    let add = yasm()
        .current_dir(workspace.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg("./missing")
        .output()
        .unwrap();

    assert!(!add.status.success());
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(stderr.contains("local source path does not exist: ./missing"));
}

#[test]
fn list_json_applies_enabled_and_agent_filters() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/enabled-skill",
        "enabled-skill",
        "Enabled skill",
        "body",
    );
    write_skill(
        source.path(),
        "skills/disabled-skill",
        "disabled-skill",
        "Disabled skill",
        "body",
    );

    for (skill, enable) in [("enabled-skill", true), ("disabled-skill", false)] {
        let mut add = yasm();
        add.env("YASM_DATA_DIR", yasm_home.path())
            .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
            .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
            .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
            .arg("add")
            .arg("--action")
            .arg("apply")
            .arg(source.path())
            .arg("--skill")
            .arg(skill);
        if enable {
            add.arg("--agent").arg("universal");
        } else {
            add.arg("--no-enable");
        }
        let result = add.output().unwrap();
        assert!(
            result.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let enabled = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("list")
        .arg("--enabled")
        .arg("--json")
        .output()
        .unwrap();
    assert!(enabled.status.success());
    let enabled: Value = serde_json::from_slice(&enabled.stdout).unwrap();
    assert!(enabled["project"].is_null());
    assert!(enabled["global"]["skills"].get("enabled-skill").is_some());
    assert!(enabled["global"]["skills"].get("disabled-skill").is_none());

    let claude = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("list")
        .arg("--agent")
        .arg("claude")
        .arg("--json")
        .output()
        .unwrap();
    assert!(claude.status.success());
    let claude: Value = serde_json::from_slice(&claude.stdout).unwrap();
    assert!(claude["project"].is_null());
    assert_eq!(claude["global"]["skills"], serde_json::json!({}));
}

#[test]
fn add_records_selected_agents_in_lockfile() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!(["claude", "universal"])
    );
    assert_eq!(
        json["skills"]["frontend-design"]["source"]["path"],
        source.path().canonicalize().unwrap().display().to_string()
    );
    assert!(json["skills"]["frontend-design"]["source"]
        .get("input")
        .is_none());
    assert!(json["skills"]["frontend-design"]["source"]
        .get("canonical")
        .is_none());
    assert!(json["skills"]["frontend-design"].get("resolved").is_none());
}

#[test]
fn project_flag_installs_into_project_store_with_relative_agent_link() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    assert!(project
        .path()
        .join(".yasm/skills/frontend-design/SKILL.md")
        .exists());
    assert!(project.path().join(".yasm/yasm.lock").exists());
    let project_link = project.path().join(".agents/skills/frontend-design");
    assert!(std::fs::symlink_metadata(&project_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_link(&project_link).unwrap(),
        std::path::Path::new("../../.yasm/skills/frontend-design")
    );
    assert!(!yasm_home.path().join("skills/frontend-design").exists());
    assert!(!agent_home
        .path()
        .join(".agents/skills/frontend-design")
        .exists());

    let list = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("Project Skills"));
}

#[test]
fn existing_dot_yasm_defaults_to_project_scope() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    assert!(project
        .path()
        .join(".yasm/skills/frontend-design/SKILL.md")
        .exists());
    assert!(!yasm_home.path().join("skills/frontend-design").exists());
}

#[test]
fn list_in_project_includes_project_and_global_skills_unless_global_is_explicit() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/global-skill",
        "global-skill",
        "Global skill",
        "global",
    );
    write_skill(
        source.path(),
        "skills/project-skill",
        "project-skill",
        "Project skill",
        "project",
    );

    for (global, skill, cwd) in [
        (true, "global-skill", project.path()),
        (false, "project-skill", project.path()),
    ] {
        let mut add = yasm();
        add.current_dir(cwd)
            .env("YASM_DATA_DIR", yasm_home.path())
            .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
            .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
            .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
            .arg("add");
        if global {
            add.arg("--global");
        }
        let add = add
            .arg("--action")
            .arg("apply")
            .arg("--no-enable")
            .arg(source.path())
            .arg("--skill")
            .arg(skill)
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }

    let mut list = yasm();
    list.current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("list");
    let output = list.output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Project Skills"));
    assert!(stdout.contains("project-skill"));
    assert!(stdout.contains("Global Skills"));
    assert!(stdout.contains("global-skill"));
    assert!(!stdout.contains("Use `--global` to manage these skills."));
    assert!(stdout.find("Project Skills").unwrap() < stdout.find("Global Skills").unwrap());

    let json = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(json.status.success());
    let json: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(json["project"]["skills"].get("project-skill").is_some());
    assert!(json["global"]["skills"].get("global-skill").is_some());

    for filter in [["--enabled", ""], ["--agent", "universal"]] {
        let mut list = yasm();
        list.current_dir(project.path())
            .env("YASM_DATA_DIR", yasm_home.path())
            .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
            .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
            .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
            .args(["list", "--json"])
            .arg(filter[0]);
        if !filter[1].is_empty() {
            list.arg(filter[1]);
        }
        let output = list.output().unwrap();
        assert!(output.status.success());
        let filtered: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(filtered["project"]["skills"], serde_json::json!({}));
        assert_eq!(filtered["global"]["skills"], serde_json::json!({}));
    }

    let global_only = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["list", "--global"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&global_only.stdout);
    assert!(stdout.contains("global-skill"));
    assert!(!stdout.contains("project-skill"));
    assert!(!stdout.contains("Use `--global` to manage these skills."));

    let global_only_json = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["list", "--global", "--json"])
        .output()
        .unwrap();
    assert!(global_only_json.status.success());
    let global_only_json: Value = serde_json::from_slice(&global_only_json.stdout).unwrap();
    assert!(global_only_json["project"].is_null());
    assert!(global_only_json["global"]["skills"]
        .get("global-skill")
        .is_some());

    let inferred_project = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["info", "global-skill"])
        .output()
        .unwrap();
    assert!(!inferred_project.status.success());
    assert!(String::from_utf8_lossy(&inferred_project.stderr)
        .contains("Run `yasm info global-skill --global` to inspect it globally."));

    let global_info = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["info", "global-skill", "--global"])
        .output()
        .unwrap();
    assert!(global_info.status.success());
    assert!(String::from_utf8_lossy(&global_info.stdout).contains("Scope: global"));
}

#[test]
fn project_mutations_report_global_only_skills_and_keep_duplicate_names_scoped() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/shared",
        "shared",
        "Shared skill",
        "body",
    );

    let global_add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["add", "--global", "--action", "apply", "--no-enable"])
        .arg(source.path())
        .args(["--skill", "shared"])
        .output()
        .unwrap();
    assert!(global_add.status.success());
    let global_lock_path = yasm_home.path().join("yasm.lock");
    let unchanged_global_lock = std::fs::read_to_string(&global_lock_path).unwrap();

    for command in ["enable", "disable", "update", "remove"] {
        let mut mutation = yasm();
        mutation
            .current_dir(project.path())
            .env("YASM_DATA_DIR", yasm_home.path())
            .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
            .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
            .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
            .arg(command)
            .arg("shared");
        match command {
            "enable" | "disable" => {
                mutation.args(["--agent", "universal"]);
            }
            "update" => {
                mutation.args(["--action", "skip"]);
            }
            "remove" => {
                mutation.arg("--all");
            }
            _ => unreachable!(),
        }
        let output = mutation.output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr
            .contains("Skill 'shared' is not installed in this project, but exists globally."));
        assert!(stderr.contains(&format!("yasm {command} shared --global")));
        assert!(!project.path().join(".yasm/yasm.lock").exists());
        assert_eq!(
            std::fs::read_to_string(&global_lock_path).unwrap(),
            unchanged_global_lock
        );
    }

    let project_add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["add", "--action", "apply", "--no-enable"])
        .arg(source.path())
        .args(["--skill", "shared"])
        .output()
        .unwrap();
    assert!(project_add.status.success());
    assert!(project.path().join(".yasm/skills/shared").exists());
    assert!(yasm_home.path().join("skills/shared").exists());

    let enable = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["enable", "shared", "--agent", "universal"])
        .output()
        .unwrap();
    assert!(enable.status.success());

    let project_lock: Value =
        serde_json::from_slice(&std::fs::read(project.path().join(".yasm/yasm.lock")).unwrap())
            .unwrap();
    let global_lock: Value =
        serde_json::from_slice(&std::fs::read(&global_lock_path).unwrap()).unwrap();
    assert_eq!(
        project_lock["skills"]["shared"]["enabled"],
        serde_json::json!(["universal"])
    );
    assert_eq!(
        global_lock["skills"]["shared"]["enabled"],
        serde_json::json!([])
    );
}

#[test]
fn global_flag_overrides_project_auto_detection() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--global")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    assert!(yasm_home
        .path()
        .join("skills/frontend-design/SKILL.md")
        .exists());
    assert!(agent_home
        .path()
        .join(".agents/skills/frontend-design")
        .exists());
    assert!(!project.path().join(".yasm/skills/frontend-design").exists());
}

#[test]
fn project_update_and_remove_use_project_store() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "old",
    );

    let add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );

    let update = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("frontend-design")
        .output()
        .unwrap();
    assert!(!update.status.success());
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains("missing input for update action"));
    assert!(stderr.contains("--action <review|apply|skip>"));
    let installed =
        std::fs::read_to_string(project.path().join(".yasm/skills/frontend-design/SKILL.md"))
            .unwrap();
    assert!(installed.contains("old"));

    let remove = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("remove")
        .arg("frontend-design")
        .arg("--all")
        .output()
        .unwrap();
    assert!(
        remove.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert!(!project.path().join(".yasm/skills/frontend-design").exists());
    assert!(!project
        .path()
        .join(".agents/skills/frontend-design")
        .exists());
}

#[test]
fn removed_project_flag_is_rejected_by_clap() {
    let result = yasm()
        .arg("list")
        .arg("--project")
        .arg("--global")
        .output()
        .unwrap();

    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("unexpected argument '--project'"));
}

#[test]
fn add_multi_skill_source_without_skill_errors_non_interactively() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );
    write_skill(
        source.path(),
        "skills/teach",
        "teach",
        "Teaching skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(!add.status.success());
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(stderr.contains("missing input for skills"));
    assert!(stderr.contains("--skill <name>"));
}

#[test]
fn add_rejects_ambiguous_skill_name_with_matching_paths() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "plugins/one/skills/shared",
        "shared",
        "First shared skill",
        "body",
    );
    write_skill(
        source.path(),
        "plugins/two/skills/shared",
        "shared",
        "Second shared skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg(source.path())
        .arg("--skill")
        .arg("shared")
        .output()
        .unwrap();

    assert!(!add.status.success());
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(stderr.contains("skill `shared` is ambiguous"));
    assert!(stderr.contains("plugins/one/skills/shared/SKILL.md"));
    assert!(stderr.contains("plugins/two/skills/shared/SKILL.md"));
    assert!(!yasm_home.path().join("skills/shared").exists());
}

#[test]
fn add_without_agent_enables_all_agents() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .output()
        .unwrap();

    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(yasm_home.path().join("skills/frontend-design").exists());
    assert!(
        std::fs::symlink_metadata(agent_home.path().join(".agents/skills/frontend-design"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::symlink_metadata(agent_home.path().join(".claude/skills/frontend-design"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!(["claude", "universal"])
    );
}

#[test]
fn add_no_enable_does_not_enable_agents() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg(source.path())
        .output()
        .unwrap();

    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let stdout = String::from_utf8_lossy(&add.stdout);
    assert!(stdout.contains("acquired frontend-design"));
    assert!(yasm_home.path().join("skills/frontend-design").exists());
    assert!(!agent_home
        .path()
        .join(".agents/skills/frontend-design")
        .exists());
    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!([])
    );
}

#[test]
fn add_no_enable_conflicts_with_agent() {
    let result = yasm()
        .arg("add")
        .arg("./skills")
        .arg("--no-enable")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();

    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("cannot be used with"));
}

#[test]
fn remove_without_skills_errors_non_interactively_when_skills_are_installed() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let remove = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("remove")
        .output()
        .unwrap();

    assert!(!remove.status.success());
    let stderr = String::from_utf8_lossy(&remove.stderr);
    assert!(stderr.contains("missing input for skills"));
    assert!(stderr.contains("pass one or more skill names"));
    assert!(agent_home
        .path()
        .join(".agents/skills/frontend-design")
        .exists());
    assert!(yasm_home.path().join("skills/frontend-design").exists());
}

#[test]
fn remove_skill_all_removes_all_agent_links_and_store_entry() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let remove = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("remove")
        .arg("frontend-design")
        .arg("--all")
        .output()
        .unwrap();

    assert!(
        remove.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert!(!agent_home
        .path()
        .join(".agents/skills/frontend-design")
        .exists());
    assert!(!agent_home
        .path()
        .join(".claude/skills/frontend-design")
        .exists());
    assert!(!yasm_home.path().join("skills/frontend-design").exists());

    let list_json = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("list")
        .arg("--json")
        .output()
        .unwrap();
    assert!(list_json.status.success());
    assert!(String::from_utf8_lossy(&list_json.stdout).contains("\"skills\": {}"));
}

#[test]
fn disable_agent_updates_lock_without_removing_store() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let disable = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("disable")
        .arg("frontend-design")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();
    assert!(
        disable.status.success(),
        "disable failed: {}",
        String::from_utf8_lossy(&disable.stderr)
    );

    assert!(agent_home
        .path()
        .join(".agents/skills/frontend-design")
        .exists());
    assert!(!agent_home
        .path()
        .join(".claude/skills/frontend-design")
        .exists());
    assert!(yasm_home.path().join("skills/frontend-design").exists());

    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!(["universal"])
    );
}

#[cfg(unix)]
#[test]
fn full_remove_unlinks_only_agents_recorded_in_lockfile() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let store_skill = yasm_home.path().join("skills/frontend-design");
    let claude_link = agent_home.path().join(".claude/skills/frontend-design");
    std::fs::create_dir_all(claude_link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store_skill, &claude_link).unwrap();

    let remove = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("remove")
        .arg("frontend-design")
        .arg("--all")
        .output()
        .unwrap();
    assert!(
        remove.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );

    assert!(!agent_home
        .path()
        .join(".agents/skills/frontend-design")
        .exists());
    assert!(std::fs::symlink_metadata(&claude_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!store_skill.exists());
}

#[cfg(unix)]
#[test]
fn remove_all_handles_agent_skill_dirs_that_resolve_to_same_directory() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    let shared_skills = agent_home.path().join("shared-skills");
    std::fs::create_dir_all(&shared_skills).unwrap();
    std::fs::create_dir_all(agent_home.path().join(".agents")).unwrap();
    std::fs::create_dir_all(agent_home.path().join(".claude")).unwrap();
    std::os::unix::fs::symlink(
        &shared_skills,
        agent_home.path().join(".agents").join("skills"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        &shared_skills,
        agent_home.path().join(".claude").join("skills"),
    )
    .unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(
        std::fs::symlink_metadata(shared_skills.join("frontend-design"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let remove = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("remove")
        .arg("frontend-design")
        .arg("--all")
        .output()
        .unwrap();
    assert!(
        remove.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let stdout = String::from_utf8_lossy(&remove.stdout);
    assert_eq!(stdout, "removed frontend-design\n");
    assert!(!shared_skills.join("frontend-design").exists());
    assert!(!yasm_home.path().join("skills/frontend-design").exists());
}

#[test]
fn update_with_changed_skill_does_not_apply_non_interactively() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    let skill_dir = source.path().join("frontend-design");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: frontend-design\ndescription: UI design skill\n---\nold\n",
    )
    .unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: frontend-design\ndescription: UI design skill\n---\nnew\n",
    )
    .unwrap();

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("frontend-design")
        .output()
        .unwrap();
    assert!(!update.status.success());
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains("missing input for update action"));
    assert!(stderr.contains("--action <review|apply|skip>"));
    assert!(!stderr.contains("Checking 1 installed skill(s)"));
    assert!(!stderr.contains("Fetching frontend-design source"));

    let installed =
        std::fs::read_to_string(yasm_home.path().join("skills/frontend-design/SKILL.md")).unwrap();
    assert!(installed.contains("old"));
}

#[test]
fn update_review_prints_similar_diff_before_non_tty_confirmation_error() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    let skill_dir = source.path().join("frontend-design");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: frontend-design\ndescription: UI design skill\n---\nold\n",
    )
    .unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: frontend-design\ndescription: UI design skill\n---\nnew\n",
    )
    .unwrap();

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("frontend-design")
        .arg("--action")
        .arg("review")
        .output()
        .unwrap();

    assert!(!update.status.success());
    let stdout = String::from_utf8_lossy(&update.stdout);
    assert!(stdout
        .contains("diff -- installed/frontend-design/SKILL.md candidate/frontend-design/SKILL.md"));
    assert!(stdout.contains("--- installed/frontend-design/SKILL.md"));
    assert!(stdout.contains("+++ candidate/frontend-design/SKILL.md"));
    assert!(stdout.contains("-old"));
    assert!(stdout.contains("+new"));
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains("missing input for update confirmation"));
}

#[test]
fn update_preserves_installed_copy_without_interactive_selection() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    let skill_dir = source.path().join("frontend-design");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: frontend-design\ndescription: UI design skill\n---\nupstream old\n",
    )
    .unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let installed_skill = yasm_home.path().join("skills/frontend-design/SKILL.md");
    std::fs::write(
        &installed_skill,
        "---\nname: frontend-design\ndescription: UI design skill\n---\ninstalled edited\n",
    )
    .unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: frontend-design\ndescription: UI design skill\n---\nupstream new\n",
    )
    .unwrap();

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("frontend-design")
        .output()
        .unwrap();
    assert!(!update.status.success());
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains("missing input for update action"));

    let installed = std::fs::read_to_string(&installed_skill).unwrap();
    assert!(installed.contains("installed edited"));
    assert!(!installed.contains("upstream new"));
}

#[test]
fn update_without_skill_prompts_to_select_multiple_changed_skills() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "old frontend",
    );
    write_skill(
        source.path(),
        "skills/teach",
        "teach",
        "Teaching skill",
        "old teach",
    );

    for skill in ["frontend-design", "teach"] {
        let add = yasm()
            .env("YASM_DATA_DIR", yasm_home.path())
            .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
            .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
            .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
            .arg("add")
            .arg("--action")
            .arg("apply")
            .arg(source.path())
            .arg("--skill")
            .arg(skill)
            .arg("--agent")
            .arg("universal")
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }

    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new frontend",
    );
    write_skill(
        source.path(),
        "skills/teach",
        "teach",
        "Teaching skill",
        "new teach",
    );

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .output()
        .unwrap();

    assert!(!update.status.success());
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains("missing input for skills"));
    assert!(stderr.contains("pass one or more skill IDs explicitly"));
    let frontend =
        std::fs::read_to_string(yasm_home.path().join("skills/frontend-design/SKILL.md")).unwrap();
    let teach = std::fs::read_to_string(yasm_home.path().join("skills/teach/SKILL.md")).unwrap();
    assert!(frontend.contains("old frontend"));
    assert!(teach.contains("old teach"));
}

#[test]
fn update_with_explicit_skill_and_action_applies_non_interactively_as_json() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    let skill_dir = source.path().join("frontend-design");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: frontend-design\ndescription: UI design skill\n---\nold\n",
    )
    .unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: frontend-design\ndescription: UI design skill\n---\nnew\n",
    )
    .unwrap();

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("frontend-design")
        .arg("--action")
        .arg("apply")
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(!stderr.contains("Fetching frontend-design source"));
    assert!(!stderr.contains("Installing frontend-design"));
    let update_json: Value = serde_json::from_slice(&update.stdout).unwrap();
    assert_eq!(
        update_json["updated"],
        serde_json::json!(["frontend-design"])
    );
    assert_eq!(update_json["skipped"], serde_json::json!([]));
    assert_eq!(update_json["failed"], serde_json::json!([]));

    let installed =
        std::fs::read_to_string(yasm_home.path().join("skills/frontend-design/SKILL.md")).unwrap();
    assert!(installed.contains("new"));
}

#[test]
fn update_ignores_uninstalled_skills_with_invalid_frontmatter() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "old",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg(source.path())
        .output()
        .unwrap();
    assert!(add.status.success());

    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let unrelated = source.path().join("skills/unrelated");
    std::fs::create_dir_all(&unrelated).unwrap();
    std::fs::write(
        unrelated.join("SKILL.md"),
        "---\nname: unrelated\ndescription: Broken: description\n---\nbody\n",
    )
    .unwrap();

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("frontend-design")
        .arg("--action")
        .arg("apply")
        .output()
        .unwrap();

    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(!stderr.contains("unrelated"));
    assert!(!stderr.contains("skipped skill"));
    let installed =
        std::fs::read_to_string(yasm_home.path().join("skills/frontend-design/SKILL.md")).unwrap();
    assert!(installed.contains("new"));
}

#[test]
fn update_does_not_relocate_a_missing_skill_by_name() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "old",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg(source.path())
        .output()
        .unwrap();
    assert!(add.status.success());

    std::fs::remove_dir_all(source.path().join("skills/frontend-design")).unwrap();
    write_skill(
        source.path(),
        "relocated/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("frontend-design")
        .arg("--action")
        .arg("apply")
        .output()
        .unwrap();

    assert!(!update.status.success());
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains("recorded path `skills/frontend-design/SKILL.md`"));
    assert!(stderr.contains("re-add the skill if it moved"));
    let installed =
        std::fs::read_to_string(yasm_home.path().join("skills/frontend-design/SKILL.md")).unwrap();
    assert!(installed.contains("old"));
}

#[test]
fn update_reports_invalid_frontmatter_for_the_installed_skill() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "old",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--no-enable")
        .arg(source.path())
        .output()
        .unwrap();
    assert!(add.status.success());

    std::fs::write(
        source.path().join("skills/frontend-design/SKILL.md"),
        "---\nname: frontend-design\ndescription: Broken: description\n---\nnew\n",
    )
    .unwrap();

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("frontend-design")
        .arg("--action")
        .arg("apply")
        .output()
        .unwrap();

    assert!(!update.status.success());
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains(
        "could not parse skill `frontend-design` at recorded path `skills/frontend-design/SKILL.md`"
    ));
    assert!(stderr.contains("YAML frontmatter error"));
    let installed =
        std::fs::read_to_string(yasm_home.path().join("skills/frontend-design/SKILL.md")).unwrap();
    assert!(installed.contains("old"));
}

#[test]
fn update_reports_failed_check_and_applies_other_updates() {
    let (yasm_home, agent_home, _source) = update_failure_fixture();

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("broken-a")
        .arg("broken-b")
        .arg("working")
        .arg("--action")
        .arg("apply")
        .output()
        .unwrap();

    assert!(!update.status.success());
    let stdout = String::from_utf8_lossy(&update.stdout);
    assert!(stdout.contains("updated working"));
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains("failed to check broken-a for updates"));
    assert!(stderr.contains("recorded path `skills/broken-a/SKILL.md`"));
    assert!(stderr.contains("failed to check broken-b for updates"));
    assert!(stderr.contains("recorded path `skills/broken-b/SKILL.md`"));
    assert!(stderr.contains("failed to check 2 skill(s) for updates"));

    let broken_a =
        std::fs::read_to_string(yasm_home.path().join("skills/broken-a/SKILL.md")).unwrap();
    assert!(broken_a.contains("old broken A"));
    let broken_b =
        std::fs::read_to_string(yasm_home.path().join("skills/broken-b/SKILL.md")).unwrap();
    assert!(broken_b.contains("old broken B"));
    let working =
        std::fs::read_to_string(yasm_home.path().join("skills/working/SKILL.md")).unwrap();
    assert!(working.contains("new working"));
}

#[test]
fn update_json_reports_failed_check_and_applies_other_updates() {
    let (yasm_home, agent_home, _source) = update_failure_fixture();

    let update = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("update")
        .arg("broken-a")
        .arg("broken-b")
        .arg("working")
        .arg("--action")
        .arg("apply")
        .arg("--json")
        .output()
        .unwrap();

    assert!(!update.status.success());
    let stdout: Value = serde_json::from_slice(&update.stdout).unwrap();
    assert_eq!(stdout["updated"], serde_json::json!(["working"]));
    assert_eq!(stdout["skipped"], serde_json::json!([]));
    assert_eq!(stdout["unchanged"], serde_json::json!([]));
    assert_eq!(stdout["failed"][0]["skill"], "broken-a");
    assert!(stdout["failed"][0]["error"]
        .as_str()
        .unwrap()
        .contains("recorded path `skills/broken-a/SKILL.md`"));
    assert_eq!(stdout["failed"][1]["skill"], "broken-b");
    assert!(stdout["failed"][1]["error"]
        .as_str()
        .unwrap()
        .contains("recorded path `skills/broken-b/SKILL.md`"));
    assert_eq!(stdout["failed"].as_array().unwrap().len(), 2);

    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(!stderr.contains("failed to check broken-a for updates"));
    assert!(!stderr.contains("failed to check broken-b for updates"));
    assert!(stderr.contains("failed to check 2 skill(s) for updates"));
    let working =
        std::fs::read_to_string(yasm_home.path().join("skills/working/SKILL.md")).unwrap();
    assert!(working.contains("new working"));
}

#[test]
fn unknown_skill_error_lists_installed_candidates() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let remove = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("remove")
        .arg("missing")
        .output()
        .unwrap();
    assert!(!remove.status.success());
    let stderr = String::from_utf8_lossy(&remove.stderr);
    assert!(stderr.contains("skill `missing` is not acquired"));
    assert!(stderr.contains("frontend-design"));
}

#[test]
fn add_without_action_errors_non_interactively() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(!add.status.success());
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(stderr.contains("missing input for add action"));
    assert!(stderr.contains("--action apply"));
    assert!(!yasm_home.path().join("skills/frontend-design").exists());
}

#[test]
fn add_replace_unmanaged_directory_requires_replace_non_interactively() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let existing = agent_home.path().join(".agents/skills/frontend-design");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("SKILL.md"), "old").unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(!add.status.success());
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(stderr.contains("--replace"));
    assert!(existing.join("SKILL.md").exists());
    assert!(std::fs::read_to_string(existing.join("SKILL.md"))
        .unwrap()
        .contains("old"));
}

#[test]
fn add_action_skip_does_not_install_or_require_replace() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let existing = agent_home.path().join(".agents/skills/frontend-design");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("SKILL.md"), "old").unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("skip")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(String::from_utf8_lossy(&add.stdout).contains("skipped frontend-design"));
    assert!(std::fs::read_to_string(existing.join("SKILL.md"))
        .unwrap()
        .contains("old"));
    assert!(!yasm_home.path().join("skills/frontend-design").exists());
}

#[test]
fn add_replace_unmanaged_directory_with_replace_installs_managed_symlink() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let existing = agent_home.path().join(".agents/skills/frontend-design");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("SKILL.md"), "old").unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--replace")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(std::fs::symlink_metadata(&existing)
        .unwrap()
        .file_type()
        .is_symlink());
    let installed = yasm_home.path().join("skills/frontend-design/SKILL.md");
    assert!(std::fs::read_to_string(installed).unwrap().contains("new"));
    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!(["universal"])
    );
}

#[test]
fn add_ignores_conflict_in_unselected_agent_path() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let claude_existing = agent_home.path().join(".claude/skills/frontend-design");
    std::fs::create_dir_all(&claude_existing).unwrap();
    std::fs::write(claude_existing.join("SKILL.md"), "old claude").unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(
        std::fs::symlink_metadata(agent_home.path().join(".agents/skills/frontend-design"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(std::fs::read_to_string(claude_existing.join("SKILL.md"))
        .unwrap()
        .contains("old claude"));
    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!(["universal"])
    );
}

#[test]
fn add_checks_conflicts_in_all_selected_agent_paths() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let claude_existing = agent_home.path().join(".claude/skills/frontend-design");
    std::fs::create_dir_all(&claude_existing).unwrap();
    std::fs::write(claude_existing.join("SKILL.md"), "old claude").unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();

    assert!(!add.status.success());
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(stderr.contains("--replace"));
    assert!(!agent_home
        .path()
        .join(".agents/skills/frontend-design")
        .exists());
    assert!(std::fs::read_to_string(claude_existing.join("SKILL.md"))
        .unwrap()
        .contains("old claude"));
}

#[test]
fn add_replace_rejects_existing_file() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let existing = agent_home.path().join(".agents/skills/frontend-design");
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, "old").unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--replace")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(!add.status.success());
    assert!(std::fs::symlink_metadata(&existing).unwrap().is_file());
    assert!(std::fs::read_to_string(&existing).unwrap().contains("old"));
}

#[cfg(unix)]
#[test]
fn add_replace_rejects_wrong_target_symlink() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let existing = agent_home.path().join(".agents/skills/frontend-design");
    let other = agent_home.path().join("other-skill");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&other, &existing).unwrap();

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--replace")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(!add.status.success());
    assert_eq!(std::fs::read_link(&existing).unwrap(), other);
}

#[test]
fn add_refreshes_existing_managed_skill_and_extends_agents() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let first_source = tempdir().unwrap();
    let second_source = tempdir().unwrap();
    write_skill(
        first_source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "old",
    );
    write_skill(
        second_source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );

    let first_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design", "--replace"])
        .arg("--action")
        .arg("apply")
        .arg(first_source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        first_add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&first_add.stderr)
    );

    let existing = agent_home.path().join(".agents/skills/frontend-design");
    assert!(std::fs::symlink_metadata(&existing)
        .unwrap()
        .file_type()
        .is_symlink());

    let second_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design", "--replace"])
        .arg("--action")
        .arg("apply")
        .arg(second_source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(
        second_add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&second_add.stderr)
    );
    assert!(String::from_utf8_lossy(&second_add.stdout).contains("updated frontend-design"));
    assert!(std::fs::symlink_metadata(&existing)
        .unwrap()
        .file_type()
        .is_symlink());
    let installed = yasm_home.path().join("skills/frontend-design/SKILL.md");
    assert!(std::fs::read_to_string(&installed).unwrap().contains("new"));

    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["source"]["path"],
        second_source
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!(["universal"])
    );

    let add_claude = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design", "--replace"])
        .arg("--action")
        .arg("apply")
        .arg(second_source.path())
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();

    assert!(
        add_claude.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add_claude.stderr)
    );
    assert!(
        std::fs::symlink_metadata(agent_home.path().join(".claude/skills/frontend-design"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!(["claude", "universal"])
    );
}

#[test]
fn add_existing_managed_skill_without_action_requires_update_selection() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "old",
    );

    let first_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design"])
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        first_add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&first_add.stderr)
    );

    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );

    let second_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design"])
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(!second_add.status.success());
    let stderr = String::from_utf8_lossy(&second_add.stderr);
    assert!(stderr.contains("missing input for managed skill updates"));
    assert!(stderr.contains("--action apply to update"));
    let installed = yasm_home.path().join("skills/frontend-design/SKILL.md");
    assert!(std::fs::read_to_string(&installed).unwrap().contains("old"));
}

#[test]
fn add_existing_managed_skill_without_diff_skips_update_selection() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let first_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design"])
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        first_add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&first_add.stderr)
    );

    let second_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design"])
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(second_add.status.success());
    assert_eq!(String::from_utf8_lossy(&second_add.stdout), "no changes\n");
    assert!(second_add.stderr.is_empty());
}

#[test]
fn add_existing_managed_skill_with_new_agent_still_requires_update_selection() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let first_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args([
            "add",
            "--skill",
            "frontend-design",
            "--action",
            "apply",
            "--agent",
            "universal",
        ])
        .arg(source.path())
        .output()
        .unwrap();
    assert!(first_add.status.success());

    let second_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .args(["add", "--skill", "frontend-design", "--agent", "claude"])
        .arg(source.path())
        .output()
        .unwrap();

    assert!(!second_add.status.success());
    assert!(String::from_utf8_lossy(&second_add.stderr)
        .contains("missing input for managed skill updates"));
}

#[test]
fn add_existing_managed_skill_without_diff_and_explicit_action_reports_no_changes() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let first_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design"])
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        first_add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&first_add.stderr)
    );

    let second_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design"])
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(
        second_add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&second_add.stderr)
    );
    assert!(String::from_utf8_lossy(&second_add.stdout)
        .trim()
        .ends_with("no changes"));
}

#[test]
fn add_existing_lock_record_still_rejects_unmanaged_agent_path_first() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let first_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design"])
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        first_add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&first_add.stderr)
    );

    let existing = agent_home.path().join(".agents/skills/frontend-design");
    std::fs::remove_file(&existing).unwrap();
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("SKILL.md"), "unmanaged").unwrap();

    let second_add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .args(["--skill", "frontend-design"])
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(!second_add.status.success());
    let stderr = String::from_utf8_lossy(&second_add.stderr);
    assert!(stderr.contains("--replace"));
    assert!(!stderr.contains("managed skill updates"));
    assert!(std::fs::read_to_string(existing.join("SKILL.md"))
        .unwrap()
        .contains("unmanaged"));
}

#[test]
fn project_add_replace_unmanaged_directory_preserves_relative_link() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "new",
    );
    let existing = project.path().join(".agents/skills/frontend-design");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("SKILL.md"), "old").unwrap();

    let add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg("--replace")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();

    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(std::fs::symlink_metadata(&existing)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_link(&existing).unwrap(),
        std::path::Path::new("../../.yasm/skills/frontend-design")
    );
    assert!(
        std::fs::read_to_string(project.path().join(".yasm/skills/frontend-design/SKILL.md"))
            .unwrap()
            .contains("new")
    );
}

#[test]
fn project_scope_from_subdirectory_uses_ancestor_store() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let nested = project.path().join("src/app");
    std::fs::create_dir_all(&nested).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let list = yasm()
        .current_dir(&nested)
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(
        list.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("Project Skills"));
    assert!(stdout.contains("frontend-design"));
    assert!(!yasm_home.path().join(".yasm").exists());
}

#[test]
fn project_store_contains_only_lock_and_skills() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let mut names = std::fs::read_dir(project.path().join(".yasm"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, vec!["skills".to_string(), "yasm.lock".to_string()]);
}

#[cfg(unix)]
#[test]
fn project_relative_links_survive_copy_to_new_path() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let project = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .current_dir(project.path())
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let copied = tempdir().unwrap();
    let dest = copied.path().join("copy");
    copy_dir(project.path(), &dest);
    let link = dest.join(".agents/skills/frontend-design");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        std::path::Path::new("../../.yasm/skills/frontend-design")
    );
    assert!(link.join("SKILL.md").exists());
}

#[test]
fn enable_without_agent_errors_non_interactively() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .output()
        .unwrap();
    assert!(add.status.success());

    let enable = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("enable")
        .arg("frontend-design")
        .output()
        .unwrap();
    assert!(!enable.status.success());
    let stderr = String::from_utf8_lossy(&enable.stderr);
    assert!(stderr.contains("missing --agent"));
    assert!(stderr.contains("universal"));
    assert!(stderr.contains("claude"));
}

#[test]
fn enable_without_skills_errors_non_interactively_when_skills_are_acquired() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );
    write_skill(
        source.path(),
        "skills/teach",
        "teach",
        "Teaching skill",
        "body",
    );

    for skill in ["frontend-design", "teach"] {
        let add = yasm()
            .env("YASM_DATA_DIR", yasm_home.path())
            .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
            .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
            .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
            .arg("add")
            .arg("--action")
            .arg("apply")
            .arg("--no-enable")
            .arg(source.path())
            .arg("--skill")
            .arg(skill)
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }

    let enable = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("enable")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();

    assert!(!enable.status.success());
    let stderr = String::from_utf8_lossy(&enable.stderr);
    assert!(stderr.contains("missing input for skills"));
    assert!(stderr.contains("pass one or more skill IDs explicitly"));
    assert!(stderr.contains("frontend-design"));
    assert!(stderr.contains("teach"));
    assert!(!agent_home
        .path()
        .join(".claude/skills/frontend-design")
        .exists());
    assert!(!agent_home.path().join(".claude/skills/teach").exists());
    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!([])
    );
    assert_eq!(json["skills"]["teach"]["enabled"], serde_json::json!([]));
}

#[test]
fn disable_without_skills_errors_non_interactively_when_skills_are_acquired() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );
    write_skill(
        source.path(),
        "skills/teach",
        "teach",
        "Teaching skill",
        "body",
    );

    for skill in ["frontend-design", "teach"] {
        let add = yasm()
            .env("YASM_DATA_DIR", yasm_home.path())
            .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
            .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
            .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
            .arg("add")
            .arg("--action")
            .arg("apply")
            .arg(source.path())
            .arg("--skill")
            .arg(skill)
            .arg("--agent")
            .arg("claude")
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }

    let disable = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("disable")
        .arg("--agent")
        .arg("claude")
        .output()
        .unwrap();

    assert!(!disable.status.success());
    let stderr = String::from_utf8_lossy(&disable.stderr);
    assert!(stderr.contains("missing input for skills"));
    assert!(stderr.contains("pass one or more skill IDs explicitly"));
    assert!(stderr.contains("frontend-design"));
    assert!(stderr.contains("teach"));
    assert!(agent_home
        .path()
        .join(".claude/skills/frontend-design")
        .exists());
    assert!(agent_home.path().join(".claude/skills/teach").exists());
    let json = list_json_value(yasm_home.path(), agent_home.path());
    assert_eq!(
        json["skills"]["frontend-design"]["enabled"],
        serde_json::json!(["claude"])
    );
    assert_eq!(
        json["skills"]["teach"]["enabled"],
        serde_json::json!(["claude"])
    );
}

#[test]
fn doctor_requires_repair_flag() {
    let doctor = yasm().arg("doctor").output().unwrap();
    assert!(!doctor.status.success());
    let stderr = String::from_utf8_lossy(&doctor.stderr);
    assert!(stderr.contains("--repair"));
    assert!(!stderr.contains("doctor is mutating"));

    let replace = yasm().arg("doctor").arg("--replace").output().unwrap();
    assert!(!replace.status.success());
    let replace_stderr = String::from_utf8_lossy(&replace.stderr);
    assert!(replace_stderr.contains("--repair"));
}

#[test]
fn remove_refuses_enabled_skill_without_all() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(add.status.success());

    let remove = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("remove")
        .arg("frontend-design")
        .output()
        .unwrap();
    assert!(!remove.status.success());
    let stderr = String::from_utf8_lossy(&remove.stderr);
    assert!(stderr.contains("still enabled"));
    assert!(stderr.contains("disable"));
    assert!(stderr.contains("--all"));
    assert!(yasm_home.path().join("skills/frontend-design").exists());
}

#[test]
fn status_reports_missing_and_plain_file_links() {
    let yasm_home = tempdir().unwrap();
    let agent_home = tempdir().unwrap();
    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "skills/frontend-design",
        "frontend-design",
        "UI design skill",
        "body",
    );

    let add = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("add")
        .arg("--action")
        .arg("apply")
        .arg(source.path())
        .arg("--agent")
        .arg("universal")
        .output()
        .unwrap();
    assert!(add.status.success());

    let link = agent_home.path().join(".agents/skills/frontend-design");
    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "not a symlink").unwrap();

    let status = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("status")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("plain file"));
    assert!(stdout.contains("claude: missing"));

    std::fs::remove_dir_all(yasm_home.path().join("skills/frontend-design")).unwrap();
    let missing_store = yasm()
        .env("YASM_DATA_DIR", yasm_home.path())
        .env("YASM_CACHE_DIR", yasm_home.path().join("cache"))
        .env("YASM_CONFIG_DIR", yasm_home.path().join("config"))
        .env("YASM_AGENT_SKILLS_ROOT", agent_home.path())
        .arg("status")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&missing_store.stdout);
    assert_eq!(stdout.matches("store missing").count(), 1, "{stdout}");
}

#[test]
fn init_adopts_existing_skill_and_records_local_ownership() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(
        &project,
        ".agents/skills/code-review",
        "code-review",
        "Review code",
        "preserved body",
    );

    let output = command
        .args(["init", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let link = project.join(".agents/skills/code-review");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("../../.yasm/skills/code-review")
    );
    assert!(
        std::fs::read_to_string(project.join(".yasm/skills/code-review/SKILL.md"))
            .unwrap()
            .contains("preserved body")
    );
    let lock: Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".yasm/yasm.lock")).unwrap())
            .unwrap();
    assert_eq!(lock["skills"]["code-review"]["source"]["kind"], "owned");
    assert_eq!(lock["skills"]["code-review"]["source"]["path"], ".");
    assert_eq!(
        lock["skills"]["code-review"]["enabled"],
        serde_json::json!(["universal"])
    );

    let list = yasm().current_dir(&project).arg("list").output().unwrap();
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("Project Skills (1)"));
    assert!(!stdout.contains("Source"));
    assert!(!stdout.contains("locally owned"));
    assert!(!stdout.contains(".yasm/skills/code-review/SKILL.md"));

    let info = yasm()
        .current_dir(&project)
        .args(["info", "code-review"])
        .output()
        .unwrap();
    assert!(info.status.success());
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(stdout.contains("Name: code-review"));
    assert!(stdout.contains("ID: code-review"));
    assert!(stdout.contains("Scope: project"));
    assert!(stdout.contains("Enabled: universal"));
    assert!(stdout.contains("Installed: .yasm/skills/code-review/SKILL.md"));
    assert!(stdout.contains("Ownership: locally owned"));
    assert!(!stdout.contains("Source root:"));

    let update = yasm()
        .current_dir(&project)
        .args(["update", "code-review"])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let stdout = String::from_utf8_lossy(&update.stdout);
    assert!(stdout.contains("skipped code-review (locally owned; no upstream)"));
    assert!(stdout.contains("no changes"));
    assert!(!String::from_utf8_lossy(&update.stderr).contains("failed to check"));

    let update_json = yasm()
        .current_dir(&project)
        .args(["update", "code-review", "--json"])
        .output()
        .unwrap();
    assert!(update_json.status.success());
    let json: Value = serde_json::from_slice(&update_json.stdout).unwrap();
    assert_eq!(json["updated"], serde_json::json!([]));
    assert_eq!(json["skipped"], serde_json::json!([]));
    assert_eq!(json["unchanged"], serde_json::json!([]));
    assert_eq!(json["failed"], serde_json::json!([]));
    assert_eq!(
        json["not_checked"],
        serde_json::json!([{
            "skill": "code-review",
            "reason": "locally owned; no upstream"
        }])
    );
}

#[test]
fn update_skips_owned_skills_and_updates_sourced_skills() {
    let mut init = yasm();
    let project = init.get_current_dir().unwrap().to_path_buf();
    write_skill(
        &project,
        ".agents/skills/owned-skill",
        "owned-skill",
        "Owned skill",
        "owned body",
    );
    let output = init.args(["init", "--action", "apply"]).output().unwrap();
    assert!(output.status.success());

    let source = tempdir().unwrap();
    write_skill(
        source.path(),
        "sourced-skill",
        "sourced-skill",
        "Sourced skill",
        "old body",
    );
    let add = yasm()
        .current_dir(&project)
        .args(["add", "--action", "apply", "--no-enable"])
        .arg(source.path())
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    write_skill(
        source.path(),
        "sourced-skill",
        "sourced-skill",
        "Sourced skill",
        "new body",
    );

    let update = yasm()
        .current_dir(&project)
        .args([
            "update",
            "owned-skill",
            "sourced-skill",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    let stdout = String::from_utf8_lossy(&update.stdout);
    assert!(stdout.contains("skipped owned-skill (locally owned; no upstream)"));
    assert!(stdout.contains("updated sourced-skill"));
    assert!(
        std::fs::read_to_string(project.join(".yasm/skills/sourced-skill/SKILL.md"))
            .unwrap()
            .contains("new body")
    );
}

#[test]
fn init_deduplicates_identical_agent_copies_and_preserves_visibility() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    for path in [".agents/skills/shared", ".claude/skills/shared"] {
        write_skill(&project, path, "shared", "Shared skill", "same body");
    }

    let output = command
        .args(["init", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for path in [".agents/skills/shared", ".claude/skills/shared"] {
        assert!(std::fs::symlink_metadata(project.join(path))
            .unwrap()
            .file_type()
            .is_symlink());
    }
    let lock: Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".yasm/yasm.lock")).unwrap())
            .unwrap();
    assert_eq!(
        lock["skills"]["shared"]["enabled"],
        serde_json::json!(["claude", "universal"])
    );
}

#[test]
fn init_reports_conflicting_copies_without_writing() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(
        &project,
        ".agents/skills/shared",
        "shared",
        "Shared",
        "first",
    );
    write_skill(
        &project,
        ".claude/skills/shared",
        "shared",
        "Shared",
        "second",
    );

    let output = command
        .args(["init", "--action", "apply"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("different contents"));
    assert!(!project.join(".yasm").exists());
    assert!(project.join(".agents/skills/shared/SKILL.md").exists());
    assert!(project.join(".claude/skills/shared/SKILL.md").exists());
}

#[test]
fn init_review_is_read_only_without_a_tty() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(&project, ".agents/skills/demo", "demo", "Demo", "body");

    let output = command
        .args(["init", "--action", "review"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Planned changes"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("review only"));
    assert!(!project.join(".yasm").exists());
    assert!(project.join(".agents/skills/demo/SKILL.md").exists());
}

#[test]
fn init_creates_nested_store_in_current_directory() {
    let mut command = yasm();
    let parent = command.get_current_dir().unwrap().to_path_buf();
    std::fs::create_dir(parent.join(".yasm")).unwrap();
    let nested = parent.join("nested/project");
    std::fs::create_dir_all(&nested).unwrap();
    command.current_dir(&nested);

    let output = command.args(["init", "--no-migrate"]).output().unwrap();
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "initialized project store at .yasm\n"
    );
    assert!(nested.join(".yasm/yasm.lock").exists());
}

#[test]
fn project_commit_hint_is_only_shown_inside_git() {
    let mut non_git = yasm();
    let non_git_root = non_git.get_current_dir().unwrap().to_path_buf();
    write_skill(&non_git_root, ".agents/skills/demo", "demo", "Demo", "body");
    let output = non_git
        .args(["init", "--action", "apply"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Commit .yasm/"));

    let mut repository = yasm();
    let repository_root = repository.get_current_dir().unwrap().to_path_buf();
    git(&repository_root, &["init"]);
    write_skill(
        &repository_root,
        ".agents/skills/demo",
        "demo",
        "Demo",
        "body",
    );
    let output = repository
        .args(["init", "--action", "apply"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("Commit .yasm/ and the agent skill links to version control."));
}

#[test]
fn migrate_apply_is_non_interactive_and_updates_the_store_links_and_lockfile() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_migration_applied(&project, &initial_lock);
}

#[test]
fn migrate_preserves_nested_git_metadata_and_records_a_healthy_digest() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    let git_metadata = project.join(".agents/skills/demo/.git");
    std::fs::create_dir_all(git_metadata.join("objects")).unwrap();
    std::fs::write(git_metadata.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(git_metadata.join("objects/example"), b"object bytes").unwrap();

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stored = project.join(".yasm/skills/demo/.git");
    assert_eq!(
        std::fs::read_to_string(stored.join("HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
    assert_eq!(
        std::fs::read(stored.join("objects/example")).unwrap(),
        b"object bytes"
    );
    let status = yasm().current_dir(&project).arg("status").output().unwrap();
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("store present; digest ok"));
}

#[test]
fn migrate_skill_argument_selects_a_subset_non_interactively() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);
    write_skill(
        &project,
        ".claude/skills/other",
        "other",
        "Other skill",
        "other body",
    );

    let output = command
        .args(["migrate", "--skill", "demo", "--action", "apply"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_migration_applied(&project, &initial_lock);
    assert!(project.join(".claude/skills/other/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/other").exists());
}

#[test]
fn project_migrate_preserves_github_source_from_skills_cli_lock() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    std::fs::create_dir_all(project.join("owner/repo")).unwrap();
    std::fs::write(
        project.join("skills-lock.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "skills": {
                "demo": {
                    "source": "owner/repo@ignored-ref",
                    "sourceType": "github",
                    "skillPath": "skills/demo/SKILL.md",
                    "computedHash": "ignored"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("github:owner/repo · skills/demo (from install history (skills-lock.json))"));
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert_eq!(lock["skills"]["demo"]["source"]["kind"], "github");
    assert_eq!(
        lock["skills"]["demo"]["source"]["path"],
        "https://github.com/owner/repo.git"
    );
    assert_eq!(lock["skills"]["demo"]["source"]["subpath"], "skills/demo");
    assert!(lock["skills"]["demo"]["source"].get("ref").is_none());
    assert!(project.join("skills-lock.json").exists());
}

#[test]
fn project_migrate_falls_back_from_invalid_github_source_url() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    std::fs::create_dir_all(project.join("local/source")).unwrap();
    std::fs::write(
        project.join("skills-lock.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "skills": {
                "demo": {
                    "source": "owner/repo",
                    "sourceUrl": "./local/source",
                    "sourceType": "github",
                    "skillPath": "skills/demo/SKILL.md"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["demo"]["source"]["path"],
        "https://github.com/owner/repo.git"
    );
}

#[test]
fn migrate_uses_catalog_after_unusable_lock_entry_and_leaves_unselected_skills_alone() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    write_skill(
        &project,
        ".agents/skills/frontend-design",
        "frontend-design",
        "Frontend design",
        "body",
    );
    std::fs::write(
        project.join("skills-lock.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "skills": {
                "frontend-design": {
                    "source": "wrong/repo",
                    "sourceType": "github",
                    "skillPath": "../SKILL.md"
                },
                "demo": {
                    "source": "owner/repo",
                    "sourceType": "github",
                    "skillPath": "skills/demo/SKILL.md"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--skill", "frontend-design", "--action", "apply"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("github:anthropics/skills · skills/frontend-design (recommended)"));
    assert!(project.join(".agents/skills/demo/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/demo").exists());
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["frontend-design"]["source"]["path"],
        "https://github.com/anthropics/skills.git"
    );
    assert!(lock["skills"].get("demo").is_none());
}

#[test]
fn usable_lock_entry_takes_precedence_over_catalog() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    std::fs::create_dir(project.join(".yasm")).unwrap();
    write_skill(
        &project,
        ".agents/skills/frontend-design",
        "frontend-design",
        "Frontend design",
        "body",
    );
    std::fs::write(
        project.join("skills-lock.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "skills": {
                "frontend-design": {
                    "source": "team/custom-skills",
                    "sourceType": "github",
                    "skillPath": "SKILL.md"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["frontend-design"]["source"]["path"],
        "https://github.com/team/custom-skills.git"
    );
    assert!(lock["skills"]["frontend-design"]["source"]
        .get("subpath")
        .is_none());
}

#[test]
fn migrate_with_upstream_leaves_unknown_candidates_unmanaged() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    write_skill(
        &project,
        ".claude/skills/frontend-design",
        "frontend-design",
        "Frontend design",
        "preserved body",
    );

    let output = command
        .args(["migrate", "--with-upstream", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(project
        .join(".yasm/skills/frontend-design/SKILL.md")
        .exists());
    assert!(project.join(".agents/skills/demo/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/demo").exists());
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["frontend-design"]["source"]["path"],
        "https://github.com/anthropics/skills.git"
    );
    assert!(lock["skills"].get("demo").is_none());
}

#[test]
fn migrate_manual_source_preserves_files_and_first_update_records_revision() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    std::fs::write(
        project.join(".agents/skills/demo/support.txt"),
        "supporting resource\n",
    )
    .unwrap();

    let remote = tempdir().unwrap();
    git(remote.path(), &["init", "--initial-branch=main"]);
    write_skill(
        remote.path(),
        "skills/demo",
        "demo",
        "Demo skill",
        "original body",
    );
    std::fs::write(
        remote.path().join("skills/demo/support.txt"),
        "supporting resource\n",
    )
    .unwrap();
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "initial"]);
    let commit = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(remote.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    redirect_github(&mut command, remote.path());
    let migrate = command
        .args([
            "migrate",
            "--skill",
            "demo",
            "--source",
            "owner/repo",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(
        migrate.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&migrate.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(project.join(".yasm/skills/demo/SKILL.md")).unwrap(),
        "---\nname: demo\ndescription: Demo skill\n---\noriginal body\n"
    );
    let lock_path = project.join(".yasm/yasm.lock");
    let lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(lock["skills"]["demo"]["source"]["subpath"], "skills/demo");
    assert!(lock["skills"]["demo"].get("resolved").is_none());

    let mut skip = yasm();
    skip.current_dir(&project);
    redirect_github(&mut skip, remote.path());
    let skipped = skip
        .args(["update", "demo", "--action", "skip"])
        .output()
        .unwrap();
    assert!(skipped.status.success());
    assert!(String::from_utf8_lossy(&skipped.stdout).contains("skipped demo"));
    let lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert!(lock["skills"]["demo"].get("resolved").is_none());

    let mut apply = yasm();
    apply.current_dir(&project);
    redirect_github(&mut apply, remote.path());
    let applied = apply
        .args(["update", "demo", "--action", "apply", "--json"])
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let summary: Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(summary["updated"], serde_json::json!([]));
    assert_eq!(summary["revision_recorded"], serde_json::json!(["demo"]));
    let lock: Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert_eq!(lock["skills"]["demo"]["resolved"]["commit"], commit);
    assert_eq!(
        std::fs::read_to_string(project.join(".yasm/skills/demo/support.txt")).unwrap(),
        "supporting resource\n"
    );
}

#[test]
fn update_review_records_unpinned_revision_without_prompt() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    let remote = tempdir().unwrap();
    git(remote.path(), &["init", "--initial-branch=main"]);
    write_skill(
        remote.path(),
        "skills/demo",
        "demo",
        "Demo skill",
        "original body",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "initial"]);
    let commit = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(remote.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    redirect_github(&mut command, remote.path());
    let migrate = command
        .args([
            "migrate",
            "--skill",
            "demo",
            "--source",
            "owner/repo",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(
        migrate.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&migrate.stderr)
    );

    let mut review = yasm();
    review.current_dir(&project);
    redirect_github(&mut review, remote.path());
    let reviewed = review
        .args(["update", "demo", "--action", "review"])
        .output()
        .unwrap();
    assert!(
        reviewed.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&reviewed.stderr)
    );
    let stdout = String::from_utf8_lossy(&reviewed.stdout);
    assert!(!stdout.contains("recorded revision"));
    assert!(!stdout.contains("Record verified revision"));
    assert!(!stdout.contains("verified revision available"));
    assert!(!String::from_utf8_lossy(&reviewed.stderr).contains("missing input"));
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert_eq!(lock["skills"]["demo"]["resolved"]["commit"], commit);
    assert_eq!(
        std::fs::read_to_string(project.join(".yasm/skills/demo/SKILL.md")).unwrap(),
        "---\nname: demo\ndescription: Demo skill\n---\noriginal body\n"
    );
}

#[test]
fn root_level_github_skill_ignores_checkout_metadata_when_updating() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    let remote = tempdir().unwrap();
    git(remote.path(), &["init", "--initial-branch=main"]);
    write_skill(remote.path(), "", "demo", "Demo skill", "original body");
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "initial"]);

    redirect_github(&mut command, remote.path());
    let migrate = command
        .args([
            "migrate",
            "--skill",
            "demo",
            "--source",
            "owner/repo",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(
        migrate.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&migrate.stderr)
    );

    let mut verify = yasm();
    verify.current_dir(&project);
    redirect_github(&mut verify, remote.path());
    let verified = verify
        .args(["update", "demo", "--action", "apply", "--json"])
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "update failed: {} {}",
        String::from_utf8_lossy(&verified.stderr),
        String::from_utf8_lossy(&verified.stdout)
    );
    let summary: Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(summary["updated"], serde_json::json!([]));
    assert_eq!(summary["revision_recorded"], serde_json::json!(["demo"]));
    assert!(!project.join(".yasm/skills/demo/.git").exists());

    std::fs::create_dir(project.join(".yasm/skills/demo/.git")).unwrap();
    std::fs::write(
        project.join(".yasm/skills/demo/.git/HEAD"),
        "stale checkout metadata\n",
    )
    .unwrap();
    let mut clean = yasm();
    clean.current_dir(&project);
    redirect_github(&mut clean, remote.path());
    let cleaned = clean
        .args(["update", "demo", "--action", "apply", "--json"])
        .output()
        .unwrap();
    assert!(
        cleaned.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&cleaned.stderr)
    );
    let summary: Value = serde_json::from_slice(&cleaned.stdout).unwrap();
    assert_eq!(summary["updated"], serde_json::json!(["demo"]));
    assert!(!project.join(".yasm/skills/demo/.git").exists());

    write_skill(remote.path(), "", "demo", "Demo skill", "updated body");
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "update"]);
    let mut update = yasm();
    update.current_dir(&project);
    redirect_github(&mut update, remote.path());
    let updated = update
        .args(["update", "demo", "--action", "apply", "--json"])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let summary: Value = serde_json::from_slice(&updated.stdout).unwrap();
    assert_eq!(summary["updated"], serde_json::json!(["demo"]));
    assert_eq!(summary["revision_recorded"], serde_json::json!([]));
    assert!(
        std::fs::read_to_string(project.join(".yasm/skills/demo/SKILL.md"))
            .unwrap()
            .contains("updated body")
    );
    assert!(!project.join(".yasm/skills/demo/.git").exists());
}

#[test]
fn migrate_source_requires_an_explicit_skill() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);

    let output = command
        .args(["migrate", "--source", "local", "--action", "apply"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("`--source` requires at least one `--skill <name>` argument"));
    assert_migration_not_applied(&project, &initial_lock);
}

#[test]
fn global_migrate_reads_only_xdg_skills_cli_lock_and_preserves_ref() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let state = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    write_skill(
        agents.path(),
        ".agents/skills/global-skill",
        "global-skill",
        "Global",
        "body",
    );
    std::fs::write(
        workspace.path().join("skills-lock.json"),
        r#"{"version":1,"skills":{"global-skill":{"source":"wrong/project","sourceType":"github","skillPath":"SKILL.md"}}}"#,
    )
    .unwrap();
    let global_lock = state.path().join("skills/.skill-lock.json");
    std::fs::create_dir_all(global_lock.parent().unwrap()).unwrap();
    std::fs::write(
        &global_lock,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 3,
            "skills": {
                "global-skill": {
                    "source": "owner/global-skills",
                    "sourceUrl": "https://github.com/owner/global-skills",
                    "sourceType": "github",
                    "ref": "release-v1",
                    "skillPath": "skills/global-skill/SKILL.md"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let output = yasm_with_roots(data.path(), agents.path())
        .current_dir(workspace.path())
        .env("XDG_STATE_HOME", state.path())
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(
        "github:owner/global-skills@release-v1 · skills/global-skill (from install history (.skill-lock.json))"
    ));
    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["global-skill"]["source"]["ref"],
        "release-v1"
    );
    assert!(global_lock.exists());
}

#[test]
fn migrate_review_is_read_only_without_a_tty() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);

    let output = command
        .args(["migrate", "--action", "review"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Planned changes"));
    assert!(stdout.contains("review only; no changes made"));
    assert_migration_not_applied(&project, &initial_lock);
}

#[test]
fn migrate_without_an_action_fails_without_a_tty() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);

    let output = command.arg("migrate").output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("missing migration action; pass `--action apply` or `--action review`"));
    assert_migration_not_applied(&project, &initial_lock);
}

#[test]
fn migrate_deduplicates_identical_agent_copies_and_preserves_both_links() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);
    write_skill(
        &project,
        ".claude/skills/demo",
        "demo",
        "Demo skill",
        "original body",
    );

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_ne!(
        std::fs::read(project.join(".yasm/yasm.lock")).unwrap(),
        initial_lock
    );
    for path in [".agents/skills/demo", ".claude/skills/demo"] {
        assert!(std::fs::symlink_metadata(project.join(path))
            .unwrap()
            .file_type()
            .is_symlink());
    }
    let lock: Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".yasm/yasm.lock")).unwrap())
            .unwrap();
    assert_eq!(
        lock["skills"]["demo"]["enabled"],
        serde_json::json!(["claude", "universal"])
    );
}

#[cfg(unix)]
#[test]
fn migrate_adopts_relative_and_absolute_aliases_of_canonical_skills() {
    use std::os::unix::fs::symlink;

    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    std::fs::create_dir(project.join(".yasm")).unwrap();
    write_skill(
        &project,
        ".agents/skills/relative",
        "relative",
        "Relative",
        "body",
    );
    write_skill(
        &project,
        ".agents/skills/absolute",
        "absolute",
        "Absolute",
        "body",
    );
    std::fs::create_dir_all(project.join(".claude/skills")).unwrap();
    symlink(
        "../../.agents/skills/relative",
        project.join(".claude/skills/relative"),
    )
    .unwrap();
    symlink(
        project.join(".agents/skills/absolute"),
        project.join(".claude/skills/absolute"),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for id in ["relative", "absolute"] {
        for root in [".agents/skills", ".claude/skills"] {
            let path = project.join(root).join(id);
            assert!(std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                std::fs::canonicalize(path).unwrap(),
                std::fs::canonicalize(project.join(".yasm/skills").join(id)).unwrap()
            );
        }
    }
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    for id in ["relative", "absolute"] {
        assert_eq!(
            lock["skills"][id]["enabled"],
            serde_json::json!(["claude", "universal"])
        );
    }
}

#[cfg(unix)]
#[test]
fn init_adopts_aliases_and_leaves_unselected_aliases_untouched() {
    use std::os::unix::fs::symlink;

    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    for id in ["selected", "other"] {
        write_skill(&project, &format!(".agents/skills/{id}"), id, id, "body");
    }
    std::fs::create_dir_all(project.join(".claude/skills")).unwrap();
    for id in ["selected", "other"] {
        symlink(
            format!("../../.agents/skills/{id}"),
            project.join(".claude/skills").join(id),
        )
        .unwrap();
    }

    let output = command
        .args(["init", "--skill", "selected", "--action", "apply"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(project.join(".yasm/skills/selected/SKILL.md").exists());
    assert!(project.join(".agents/skills/other/SKILL.md").exists());
    assert_eq!(
        std::fs::read_link(project.join(".claude/skills/other")).unwrap(),
        std::path::PathBuf::from("../../.agents/skills/other")
    );
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["selected"]["enabled"],
        serde_json::json!(["claude", "universal"])
    );
    assert!(lock["skills"].get("other").is_none());
}

#[cfg(unix)]
#[test]
fn migrate_leaves_dangling_and_basename_mismatched_symlinks_untouched() {
    use std::os::unix::fs::symlink;

    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    std::fs::create_dir_all(project.join(".claude/skills")).unwrap();
    symlink(
        "../../.agents/skills/missing",
        project.join(".claude/skills/dangling"),
    )
    .unwrap();
    symlink(
        "../../.agents/skills/demo",
        project.join(".claude/skills/different-name"),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        std::fs::read_link(project.join(".claude/skills/dangling")).unwrap(),
        std::path::PathBuf::from("../../.agents/skills/missing")
    );
    assert_eq!(
        std::fs::read_link(project.join(".claude/skills/different-name")).unwrap(),
        std::path::PathBuf::from("../../.agents/skills/demo")
    );
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["demo"]["enabled"],
        serde_json::json!(["universal"])
    );
}

#[test]
fn migrate_rejects_conflicting_agent_copies_without_changes() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);
    write_skill(
        &project,
        ".claude/skills/demo",
        "demo",
        "Demo skill",
        "different body",
    );

    let review = command
        .args(["migrate", "--action", "review"])
        .output()
        .unwrap();

    assert!(review.status.success());
    assert!(String::from_utf8_lossy(&review.stdout).contains("CONFLICT"));
    assert!(String::from_utf8_lossy(&review.stdout).contains("different contents"));
    let output = yasm()
        .current_dir(&project)
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("different contents"));
    assert_migration_not_applied(&project, &initial_lock);
    assert!(project.join(".claude/skills/demo/SKILL.md").exists());
}

#[cfg(unix)]
#[test]
fn migrate_adopts_an_identical_managed_copy_and_preserves_lock_metadata() {
    use std::os::unix::fs::{symlink, MetadataExt};

    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(
        &project,
        ".agents/skills/demo",
        "demo",
        "Discovered name",
        "same body",
    );
    write_skill(
        &project,
        ".yasm/skills/demo",
        "demo",
        "Discovered name",
        "same body",
    );
    std::fs::create_dir_all(project.join(".claude/skills")).unwrap();
    symlink(
        "../../.agents/skills/demo",
        project.join(".claude/skills/demo"),
    )
    .unwrap();
    let original_record = serde_json::json!({
        "name": "Locked name",
        "source": {
            "kind": "github",
            "path": "https://github.com/example/skills.git",
            "ref": "stable",
            "subpath": "skills/demo"
        },
        "resolved": {
            "ref": "stable",
            "commit": "0123456789abcdef0123456789abcdef01234567"
        },
        "skill_path": "skills/demo/SKILL.md",
        "digest": "recorded-stale-digest",
        "enabled": ["claude"]
    });
    std::fs::write(
        project.join(".yasm/yasm.lock"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 3,
            "skills": { "demo": original_record }
        }))
        .unwrap(),
    )
    .unwrap();
    let store = project.join(".yasm/skills/demo");
    let store_inode = std::fs::metadata(&store).unwrap().ino();

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("adopt existing"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("adopted demo using existing store"));
    assert_eq!(std::fs::metadata(&store).unwrap().ino(), store_inode);
    assert!(
        std::fs::symlink_metadata(project.join(".agents/skills/demo"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(project.join(".claude/skills/demo")).unwrap(),
        std::path::PathBuf::from("../../.yasm/skills/demo")
    );
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    let record = &lock["skills"]["demo"];
    assert_eq!(record["name"], "Locked name");
    assert_eq!(record["source"], original_record["source"]);
    assert_eq!(record["resolved"], original_record["resolved"]);
    assert_eq!(record["skill_path"], original_record["skill_path"]);
    assert_eq!(record["digest"], "recorded-stale-digest");
    assert_eq!(
        record["enabled"],
        serde_json::json!(["claude", "universal"])
    );

    let repeat = yasm()
        .current_dir(&project)
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();
    assert!(repeat.status.success());
    assert!(String::from_utf8_lossy(&repeat.stdout).contains("no unmanaged skills found"));
}

#[cfg(unix)]
#[test]
fn interactive_migrate_does_not_offer_source_choices_for_declined_adoption() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(&project, ".agents/skills/demo", "demo", "Demo", "same");
    write_skill(&project, ".yasm/skills/demo", "demo", "Demo", "same");
    let original_lock = serde_json::to_vec_pretty(&serde_json::json!({
        "version": 3,
        "skills": {
            "demo": {
                "name": "Demo",
                "source": {
                    "kind": "github",
                    "path": "https://github.com/example/skills.git",
                    "subpath": "skills/demo"
                },
                "skill_path": "skills/demo/SKILL.md",
                "digest": "recorded-digest",
                "enabled": []
            }
        }
    }))
    .unwrap();
    std::fs::write(project.join(".yasm/yasm.lock"), &original_lock).unwrap();
    command.arg("migrate");
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session
        .expect("Select existing managed skills to reconcile")
        .unwrap();
    session.send(" ").unwrap();
    session.send_line("").unwrap();
    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.expect("Finish for now").unwrap();
    session.send_line("").unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert!(project.join(".agents/skills/demo/SKILL.md").is_file());
    assert_eq!(
        std::fs::read(project.join(".yasm/yasm.lock")).unwrap(),
        original_lock
    );
}

#[cfg(unix)]
#[test]
fn interactive_migrate_reconciles_an_owned_managed_copy() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(&project, ".agents/skills/demo", "demo", "Demo", "same");
    write_skill(&project, ".yasm/skills/demo", "demo", "Demo", "same");
    std::fs::write(
        project.join(".yasm/yasm.lock"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 3,
            "skills": {
                "demo": {
                    "name": "Demo",
                    "source": { "kind": "owned", "path": "." },
                    "skill_path": "SKILL.md",
                    "digest": "recorded-digest",
                    "enabled": []
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    command.arg("migrate");
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session
        .expect("Select existing managed skills to reconcile")
        .unwrap();
    session.send_line("").unwrap();
    session
        .expect("Reconciled 1 existing managed skill.")
        .unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert!(
        std::fs::symlink_metadata(project.join(".agents/skills/demo"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert_eq!(lock["skills"]["demo"]["source"]["kind"], "owned");
    assert_eq!(
        lock["skills"]["demo"]["enabled"],
        serde_json::json!(["universal"])
    );
}

#[cfg(unix)]
#[test]
fn interactive_migrate_only_offers_finish_for_conflicts() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(&project, ".agents/skills/demo", "demo", "Demo", "external");
    write_skill(&project, ".yasm/skills/demo", "demo", "Demo", "stored");
    let original_lock = serde_json::to_vec_pretty(&serde_json::json!({
        "version": 3,
        "skills": {
            "demo": {
                "name": "Demo",
                "source": { "kind": "owned", "path": "." },
                "skill_path": "SKILL.md",
                "digest": "recorded-digest",
                "enabled": []
            }
        }
    }))
    .unwrap();
    std::fs::write(project.join(".yasm/yasm.lock"), &original_lock).unwrap();
    command.arg("migrate");
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.expect("Finish for now").unwrap();
    session.send_line("").unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert!(project.join(".agents/skills/demo/SKILL.md").is_file());
    assert_eq!(
        std::fs::read(project.join(".yasm/yasm.lock")).unwrap(),
        original_lock
    );
}

#[test]
fn migrate_reviews_managed_content_conflicts_and_subset_bypasses_them() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(&project, ".agents/skills/demo", "demo", "Demo", "external");
    write_skill(&project, ".yasm/skills/demo", "demo", "Demo", "stored");
    write_skill(&project, ".claude/skills/other", "other", "Other", "other");
    std::fs::write(
        project.join(".yasm/yasm.lock"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 3,
            "skills": {
                "demo": {
                    "name": "Demo",
                    "source": { "kind": "owned", "path": "." },
                    "skill_path": "SKILL.md",
                    "digest": "stale",
                    "enabled": []
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let review = command
        .args(["migrate", "--action", "review"])
        .output()
        .unwrap();
    assert!(review.status.success());
    let stdout = String::from_utf8_lossy(&review.stdout);
    assert!(stdout.contains("CONFLICT"));
    assert!(stdout.contains(".agents/skills/demo"));
    assert!(stdout.contains(".yasm/skills/demo"));

    let apply = yasm()
        .current_dir(&project)
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();
    assert!(!apply.status.success());
    assert!(project.join(".claude/skills/other/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/other").exists());

    let subset = yasm()
        .current_dir(&project)
        .args(["migrate", "--skill", "other", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        subset.status.success(),
        "{}",
        String::from_utf8_lossy(&subset.stderr)
    );
    assert!(project.join(".yasm/skills/other/SKILL.md").exists());
    assert!(project.join(".agents/skills/demo/SKILL.md").exists());
}

#[test]
fn migrate_reports_missing_store_and_missing_lock_separately() {
    for missing_store in [true, false] {
        let mut command = yasm();
        let project = command.get_current_dir().unwrap().to_path_buf();
        write_skill(&project, ".agents/skills/demo", "demo", "Demo", "source");
        std::fs::create_dir_all(project.join(".yasm")).unwrap();
        if missing_store {
            std::fs::write(
                project.join(".yasm/yasm.lock"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "version": 3,
                    "skills": {
                        "demo": {
                            "name": "Demo",
                            "source": { "kind": "owned", "path": "." },
                            "skill_path": "SKILL.md",
                            "digest": "stale",
                            "enabled": []
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        } else {
            write_skill(&project, ".yasm/skills/demo", "demo", "Demo", "source");
            std::fs::write(
                project.join(".yasm/yasm.lock"),
                b"{\n  \"version\": 3,\n  \"skills\": {}\n}\n",
            )
            .unwrap();
        }

        let output = command
            .args(["migrate", "--action", "review"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("CONFLICT"));
        if missing_store {
            assert!(stdout.contains("stored directory"));
            assert!(stdout.contains("is missing"));
        } else {
            assert!(stdout.contains("without a lockfile record"));
        }
    }
}

#[cfg(unix)]
#[test]
fn interactive_migrate_keeps_a_selected_remaining_skill_as_local() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);
    write_skill(
        &project,
        ".claude/skills/other",
        "other",
        "Other skill",
        "other body",
    );
    command.arg("migrate");
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.send_line("").unwrap();
    session.expect("Select skills to keep as local").unwrap();
    session.send(" ").unwrap();
    session.send_line("").unwrap();
    session.expect("migrated demo").unwrap();
    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.send("\x1b[B\x1b[B").unwrap();
    session.send_line("").unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert_migration_applied(&project, &initial_lock);
    assert!(project.join(".claude/skills/other/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/other").exists());
    let lock: Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".yasm/yasm.lock")).unwrap())
            .unwrap();
    assert!(lock["skills"].get("other").is_none());
}

#[cfg(unix)]
#[test]
fn interactive_migrate_can_finish_without_changes() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);
    command.arg("migrate");
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.send("\x1b[B\x1b[B").unwrap();
    session.send_line("").unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert_migration_not_applied(&project, &initial_lock);
}

#[cfg(unix)]
#[test]
fn interactive_migrate_empty_selection_makes_no_changes() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);
    command.arg("migrate");
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.send_line("").unwrap();
    session.expect("Select skills to keep as local").unwrap();
    session.send_line("").unwrap();
    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.send("\x1b[B\x1b[B").unwrap();
    session.send_line("").unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert_migration_not_applied(&project, &initial_lock);
}

#[cfg(unix)]
#[test]
fn interactive_migrate_applies_local_selection_without_confirmation() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);
    command.arg("migrate");
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.send_line("").unwrap();
    session.expect("Select skills to keep as local").unwrap();
    session.send(" ").unwrap();
    session.send_line("").unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert_migration_applied(&project, &initial_lock);
}

#[cfg(unix)]
#[test]
fn interactive_migrate_accepts_upstreams_before_handling_remaining_skills() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    write_skill(
        &project,
        ".claude/skills/frontend-design",
        "frontend-design",
        "Frontend design",
        "preserved body",
    );
    command.arg("migrate");
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session
        .expect("Select skills to migrate with these upstreams")
        .unwrap();
    session.send_line("").unwrap();
    session
        .expect("Migrated 1 skill with upstream sources.")
        .unwrap();
    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.send("\x1b[B\x1b[B").unwrap();
    session.send_line("").unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert!(project
        .join(".yasm/skills/frontend-design/SKILL.md")
        .exists());
    assert!(project.join(".agents/skills/demo/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/demo").exists());
}

#[cfg(unix)]
#[test]
fn interactive_stage_two_failure_preserves_completed_upstream_batch() {
    let mut command = yasm();
    let (project, _) = prepare_migration(&command);
    write_skill(
        &project,
        ".claude/skills/frontend-design",
        "frontend-design",
        "Frontend design",
        "preserved body",
    );
    let remote = tempdir().unwrap();
    git(remote.path(), &["init", "--initial-branch=main"]);
    write_skill(
        remote.path(),
        "skills/other",
        "other",
        "Other skill",
        "body",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "initial"]);
    redirect_github(&mut command, remote.path());
    command.arg("migrate");
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session
        .expect("Select skills to migrate with these upstreams")
        .unwrap();
    session.send_line("").unwrap();
    session
        .expect("Migrated 1 skill with upstream sources.")
        .unwrap();
    session
        .expect("What would you like to do with the remaining skills?")
        .unwrap();
    session.send("\x1b[B").unwrap();
    session.send_line("").unwrap();
    session
        .expect("Choose a skill to give a Git source")
        .unwrap();
    session.send_line("").unwrap();
    session
        .expect("Git repository address or GitHub skill-directory URL")
        .unwrap();
    session.send_line("owner/repo").unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(!matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert!(project
        .join(".yasm/skills/frontend-design/SKILL.md")
        .exists());
    assert!(project.join(".agents/skills/demo/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/demo").exists());
    let lock: Value =
        serde_json::from_slice(&std::fs::read(project.join(".yasm/yasm.lock")).unwrap()).unwrap();
    assert!(lock["skills"].get("frontend-design").is_some());
    assert!(lock["skills"].get("demo").is_none());
}

#[cfg(unix)]
#[test]
fn explicit_migrate_review_is_read_only_with_a_tty() {
    let mut command = yasm();
    let (project, initial_lock) = prepare_migration(&command);
    command.args(["migrate", "--action", "review"]);
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();

    session.expect("review only; no changes made").unwrap();
    session.expect(Eof).unwrap();
    let status = session.get_process().wait().unwrap();

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert_migration_not_applied(&project, &initial_lock);
}

#[test]
fn migrate_from_subdirectory_uses_selected_store_root() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    std::fs::create_dir(project.join(".yasm")).unwrap();
    let nested = project.join("src/app");
    std::fs::create_dir_all(&nested).unwrap();
    write_skill(
        &project,
        ".agents/skills/root-skill",
        "root-skill",
        "Root",
        "root",
    );
    write_skill(
        &nested,
        ".agents/skills/nested-skill",
        "nested-skill",
        "Nested",
        "nested",
    );
    command.current_dir(&nested);

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(project.join(".yasm/skills/root-skill/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/nested-skill").exists());
    assert!(nested.join(".agents/skills/nested-skill/SKILL.md").exists());
}

#[test]
fn migrate_without_project_defaults_to_global_locations() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    write_skill(
        agents.path(),
        ".agents/skills/global-skill",
        "global-skill",
        "Global",
        "body",
    );
    let output = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(data.path().join("skills/global-skill/SKILL.md").exists());
    assert!(
        std::fs::symlink_metadata(agents.path().join(".agents/skills/global-skill"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("Scope:"));
    assert!(!stdout.contains("Root:"));
    assert!(!stdout.contains("Sources:"));
    assert!(!stdout.contains("Destination:"));

    let update = yasm()
        .env("YASM_DATA_DIR", data.path())
        .env("YASM_AGENT_SKILLS_ROOT", agents.path())
        .args(["update", "global-skill"])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert!(String::from_utf8_lossy(&update.stdout)
        .contains("skipped global-skill (locally owned; no upstream)"));
}

#[cfg(unix)]
#[test]
fn global_migrate_adopts_agent_aliases() {
    use std::os::unix::fs::symlink;

    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    write_skill(
        agents.path(),
        ".agents/skills/global-skill",
        "global-skill",
        "Global",
        "body",
    );
    std::fs::create_dir_all(agents.path().join(".claude/skills")).unwrap();
    symlink(
        "../../.agents/skills/global-skill",
        agents.path().join(".claude/skills/global-skill"),
    )
    .unwrap();

    let output = yasm_with_roots(data.path(), agents.path())
        .args(["migrate", "--global", "--action", "apply"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["global-skill"]["enabled"],
        serde_json::json!(["claude", "universal"])
    );
    for root in [".agents/skills", ".claude/skills"] {
        assert!(
            std::fs::symlink_metadata(agents.path().join(root).join("global-skill"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[cfg(unix)]
#[test]
fn global_migrate_follows_linked_agent_directories_and_deduplicates_shared_locations() {
    use std::os::unix::fs::symlink;

    for linked_parent in [false, true] {
        for shared in [false, true] {
            let data = tempdir().unwrap();
            let agents = tempdir().unwrap();
            let external = tempdir().unwrap();
            write_skill(
                external.path(),
                "universal/skills/demo",
                "demo",
                "Demo",
                "original",
            );
            if !shared {
                write_skill(
                    external.path(),
                    "claude/skills/demo",
                    "demo",
                    "Demo",
                    "original",
                );
            }
            for (name, target) in [
                (".agents", "universal"),
                (".claude", if shared { "universal" } else { "claude" }),
            ] {
                if linked_parent {
                    let target = if shared && name == ".claude" {
                        std::path::PathBuf::from(".agents")
                    } else {
                        external.path().join(target)
                    };
                    symlink(target, agents.path().join(name)).unwrap();
                } else {
                    std::fs::create_dir(agents.path().join(name)).unwrap();
                    let target = if shared && name == ".claude" {
                        std::path::PathBuf::from("../.agents/skills")
                    } else {
                        external.path().join(target).join("skills")
                    };
                    symlink(target, agents.path().join(name).join("skills")).unwrap();
                }
            }

            let review = yasm_with_roots(data.path(), agents.path())
                .args(["migrate", "--global", "--action", "review"])
                .output()
                .unwrap();
            assert!(
                review.status.success(),
                "{}",
                String::from_utf8_lossy(&review.stderr)
            );
            let stdout = String::from_utf8_lossy(&review.stdout);
            assert!(stdout.contains(".agents/skills/demo"));
            assert!(stdout.contains(".claude/skills/demo"));
            assert!(!data.path().join("yasm.lock").exists());
            assert!(
                !std::fs::symlink_metadata(external.path().join("universal/skills/demo"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );

            let apply = yasm_with_roots(data.path(), agents.path())
                .args(["migrate", "--global", "--action", "apply"])
                .output()
                .unwrap();
            assert!(
                apply.status.success(),
                "{}",
                String::from_utf8_lossy(&apply.stderr)
            );
            let lock: Value =
                serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap())
                    .unwrap();
            assert_eq!(
                lock["skills"]["demo"]["enabled"],
                serde_json::json!(["claude", "universal"])
            );
            for name in [".agents", ".claude"] {
                let skill_dir = agents.path().join(name).join("skills");
                assert_eq!(
                    std::fs::read_link(skill_dir.join("demo")).unwrap(),
                    data.path().join("skills/demo")
                );
                assert!(std::fs::read_to_string(skill_dir.join("demo/SKILL.md"))
                    .unwrap()
                    .contains("original"));
                assert_eq!(std::fs::read_dir(&skill_dir).unwrap().count(), 1);
                let linked_path = if linked_parent {
                    agents.path().join(name)
                } else {
                    skill_dir
                };
                assert!(std::fs::symlink_metadata(linked_path)
                    .unwrap()
                    .file_type()
                    .is_symlink());
            }
            let repeat = yasm_with_roots(data.path(), agents.path())
                .args(["migrate", "--global", "--action", "apply"])
                .output()
                .unwrap();
            assert!(
                repeat.status.success(),
                "{}",
                String::from_utf8_lossy(&repeat.stderr)
            );
            assert!(String::from_utf8_lossy(&repeat.stdout).contains("no unmanaged skills found"));
        }
    }
}

#[cfg(unix)]
#[test]
fn global_migrate_does_not_replace_a_store_exposed_as_an_agent_directory() {
    use std::os::unix::fs::{symlink, MetadataExt};

    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    write_skill(data.path(), "skills/demo", "demo", "Demo", "stored");
    std::fs::write(
        data.path().join("yasm.lock"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 3,
            "skills": {
                "demo": {
                    "name": "Demo",
                    "source": { "kind": "owned", "path": "." },
                    "skill_path": "SKILL.md",
                    "digest": "recorded",
                    "enabled": ["universal"]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::create_dir(agents.path().join(".agents")).unwrap();
    symlink(
        data.path().join("skills"),
        agents.path().join(".agents/skills"),
    )
    .unwrap();
    let store = data.path().join("skills/demo");
    let inode = std::fs::metadata(&store).unwrap().ino();

    let output = yasm_with_roots(data.path(), agents.path())
        .args(["migrate", "--global", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("no unmanaged skills found"));
    assert_eq!(std::fs::metadata(&store).unwrap().ino(), inode);
    assert!(!std::fs::symlink_metadata(&store)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(
        std::fs::symlink_metadata(agents.path().join(".agents/skills"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn global_migrate_adopts_external_copy_while_store_is_another_agent_location() {
    use std::os::unix::fs::{symlink, MetadataExt};

    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let external = tempdir().unwrap();
    write_skill(data.path(), "skills/demo", "demo", "Demo", "same");
    write_skill(external.path(), "skills/demo", "demo", "Demo", "same");
    std::fs::write(
        data.path().join("yasm.lock"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 3,
            "skills": {
                "demo": {
                    "name": "Demo",
                    "source": { "kind": "owned", "path": "." },
                    "skill_path": "SKILL.md",
                    "digest": "recorded",
                    "enabled": ["universal"]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    for (agent, target) in [
        (".agents", data.path().join("skills")),
        (".claude", external.path().join("skills")),
    ] {
        std::fs::create_dir(agents.path().join(agent)).unwrap();
        symlink(target, agents.path().join(agent).join("skills")).unwrap();
    }
    let store = data.path().join("skills/demo");
    let inode = std::fs::metadata(&store).unwrap().ino();

    let output = yasm_with_roots(data.path(), agents.path())
        .args(["migrate", "--global", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::metadata(&store).unwrap().ino(), inode);
    assert!(!std::fs::symlink_metadata(&store)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(
        std::fs::symlink_metadata(external.path().join("skills/demo"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["demo"]["enabled"],
        serde_json::json!(["claude", "universal"])
    );
}

#[cfg(unix)]
#[test]
fn global_migrate_adopts_a_shared_external_copy_once() {
    use std::os::unix::fs::{symlink, MetadataExt};

    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let external = tempdir().unwrap();
    write_skill(data.path(), "skills/demo", "demo", "Demo", "same");
    write_skill(external.path(), "skills/demo", "demo", "Demo", "same");
    std::fs::write(
        data.path().join("yasm.lock"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 3,
            "skills": {
                "demo": {
                    "name": "Demo",
                    "source": { "kind": "owned", "path": "." },
                    "skill_path": "SKILL.md",
                    "digest": "recorded",
                    "enabled": []
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    for agent in [".agents", ".claude"] {
        std::fs::create_dir(agents.path().join(agent)).unwrap();
        symlink(
            external.path().join("skills"),
            agents.path().join(agent).join("skills"),
        )
        .unwrap();
    }
    let store = data.path().join("skills/demo");
    let inode = std::fs::metadata(&store).unwrap().ino();

    let output = yasm_with_roots(data.path(), agents.path())
        .args(["migrate", "--global", "--action", "apply"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::metadata(&store).unwrap().ino(), inode);
    assert!(
        std::fs::symlink_metadata(external.path().join("skills/demo"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    for agent in [".agents", ".claude"] {
        assert!(
            std::fs::symlink_metadata(agents.path().join(agent).join("skills"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    let lock: Value =
        serde_json::from_slice(&std::fs::read(data.path().join("yasm.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["skills"]["demo"]["enabled"],
        serde_json::json!(["claude", "universal"])
    );
}

#[cfg(unix)]
#[test]
fn global_migrate_reports_broken_directory_symlinks() {
    use std::os::unix::fs::symlink;

    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    symlink("missing", agents.path().join(".agents")).unwrap();
    let output = yasm_with_roots(data.path(), agents.path())
        .args(["migrate", "--global", "--action", "review"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("does not resolve to an existing directory"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("some paths were skipped"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("no unmanaged skills found"));
}

#[cfg(unix)]
#[test]
fn global_migrate_recovery_checks_linked_directory_target() {
    use std::os::unix::fs::symlink;

    for retargeted in [false, true] {
        let data = tempdir().unwrap();
        let agents = tempdir().unwrap();
        let external = tempdir().unwrap();
        let replacement = tempdir().unwrap();
        let backup_relative = "skills/.yasm-migrate-backup-test/skill";
        write_skill(external.path(), backup_relative, "demo", "Demo", "original");
        write_skill(data.path(), "skills/demo", "demo", "Demo", "original");
        let target = data.path().join("skills/demo");
        symlink(&target, external.path().join("skills/demo")).unwrap();
        symlink(external.path(), agents.path().join(".agents")).unwrap();
        let journal = serde_json::json!({
            "initializing": false,
            "lock_existed": false,
            "original_lock": { "version": 3, "skills": {} },
            "changes": [{
                "skill_id": "demo",
                "store_created": true,
                "locations": [{
                    "source": agents.path().join(".agents/skills/demo"),
                    "backup": agents.path().join(".agents").join(backup_relative),
                    "expected_target": target,
                    "original_was_symlink": false,
                    "resolved_parent": external.path().join("skills").canonicalize().unwrap()
                }]
            }]
        });
        let journal_path = data.path().join("migration-journal.json");
        std::fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        if retargeted {
            write_skill(
                replacement.path(),
                "skills/demo",
                "demo",
                "Demo",
                "unrelated",
            );
            std::fs::remove_file(agents.path().join(".agents")).unwrap();
            symlink(replacement.path(), agents.path().join(".agents")).unwrap();
        }

        let output = yasm_with_roots(data.path(), agents.path())
            .args(["migrate", "--global", "--action", "review"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if retargeted {
            assert!(!output.status.success());
            assert!(stderr.contains("changed target"), "{stderr}");
            assert!(journal_path.exists());
            assert!(target.join("SKILL.md").exists());
            assert!(external
                .path()
                .join(backup_relative)
                .join("SKILL.md")
                .exists());
            assert!(
                std::fs::read_to_string(replacement.path().join("skills/demo/SKILL.md"))
                    .unwrap()
                    .contains("unrelated")
            );
        } else {
            assert!(output.status.success(), "{stderr}");
            assert!(stderr.contains("recovered an interrupted migration"));
            assert!(!journal_path.exists());
            assert!(!target.exists());
            assert!(
                std::fs::read_to_string(external.path().join("skills/demo/SKILL.md"))
                    .unwrap()
                    .contains("original")
            );
            assert!(
                !std::fs::symlink_metadata(external.path().join("skills/demo"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(!external.path().join(backup_relative).exists());
        }
    }
}

#[test]
fn repeated_init_fails_and_migrate_is_idempotent() {
    let mut first = yasm();
    let project = first.get_current_dir().unwrap().to_path_buf();
    write_skill(&project, ".agents/skills/demo", "demo", "Demo", "body");
    assert!(first
        .args(["init", "--action", "apply"])
        .output()
        .unwrap()
        .status
        .success());

    let second = yasm()
        .current_dir(&project)
        .args(["init", "--no-migrate"])
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("yasm migrate"));

    let repeat = yasm()
        .current_dir(&project)
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();
    assert!(repeat.status.success());
    assert!(String::from_utf8_lossy(&repeat.stdout).contains("no unmanaged skills found"));
}

#[test]
fn init_without_candidates_still_requires_non_tty_action() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    let output = command.arg("init").output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--no-migrate"));
    assert!(!project.join(".yasm").exists());
}

#[test]
fn init_skill_selects_a_subset_non_interactively() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(&project, ".agents/skills/one", "one", "One", "one");
    write_skill(&project, ".agents/skills/two", "two", "Two", "two");

    let output = command
        .args(["init", "--skill", "two", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(project.join(".yasm/skills/two/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/one").exists());
    assert!(project.join(".agents/skills/one/SKILL.md").exists());
}

#[test]
fn init_rejects_the_configured_home_directory() {
    let mut command = yasm();
    let sandbox = command
        .get_current_dir()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let home = sandbox.join("home");
    command.current_dir(&home);

    let output = command.args(["init", "--no-migrate"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("migrate --global"));
    assert!(!home.join(".yasm").exists());
}

#[cfg(unix)]
#[test]
fn migrate_does_not_follow_symlinked_agent_roots_or_skill_files() {
    use std::os::unix::fs::symlink;

    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    std::fs::create_dir(project.join(".yasm")).unwrap();
    let external = tempdir().unwrap();
    write_skill(
        external.path(),
        "skills/escaped",
        "escaped",
        "Escaped",
        "external",
    );
    symlink(external.path(), project.join(".agents")).unwrap();

    let real_skill = project.join(".claude/skills/linked-file");
    std::fs::create_dir_all(&real_skill).unwrap();
    symlink(
        external.path().join("escaped/SKILL.md"),
        real_skill.join("SKILL.md"),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("agent skill directory contains a symlinked path component"));
    assert!(stderr.contains("SKILL.md is a symlink"));
    assert!(external.path().join("skills/escaped/SKILL.md").exists());
    assert!(!project.join(".yasm/skills/escaped").exists());
}

#[cfg(unix)]
#[test]
fn duplicate_detection_includes_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    for path in [".agents/skills/shared", ".claude/skills/shared"] {
        write_skill(&project, path, "shared", "Shared", "body");
    }
    std::fs::set_permissions(
        project.join(".claude/skills/shared/SKILL.md"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let output = command
        .args(["init", "--action", "apply"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("different contents"));
    assert!(!project.join(".yasm").exists());
}

#[cfg(unix)]
#[test]
fn migrate_recovers_an_interrupted_journal_before_planning() {
    use std::os::unix::fs::symlink;

    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    let source = project.join(".agents/skills/demo");
    write_skill(&project, ".agents/skills/demo", "demo", "Demo", "original");
    let backup_root = project.join(".agents/skills/.yasm-migrate-backup-test");
    let backup = backup_root.join("skill");
    std::fs::create_dir_all(&backup_root).unwrap();
    std::fs::rename(&source, &backup).unwrap();
    std::fs::create_dir_all(project.join(".yasm/skills/demo")).unwrap();
    std::fs::copy(
        backup.join("SKILL.md"),
        project.join(".yasm/skills/demo/SKILL.md"),
    )
    .unwrap();
    symlink("../../.yasm/skills/demo", &source).unwrap();
    let journal = serde_json::json!({
        "initializing": false,
        "lock_existed": false,
        "original_lock": { "version": 3, "skills": {} },
        "changes": [{
            "skill_id": "demo",
            "store_created": true,
            "locations": [{
                "source": source,
                "backup": backup,
                "expected_target": "../../.yasm/skills/demo",
                "original_was_symlink": false,
                "resolved_parent": source.parent().unwrap().canonicalize().unwrap()
            }]
        }]
    });
    std::fs::write(
        project.join(".yasm/migration-journal.json"),
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "review"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "recovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("recovered an interrupted migration"));
    assert!(source.join("SKILL.md").exists());
    assert!(!std::fs::symlink_metadata(&source)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!project.join(".yasm/skills/demo").exists());
    assert!(!project.join(".yasm/migration-journal.json").exists());
}

#[cfg(unix)]
#[test]
fn migrate_recovery_keeps_a_store_reused_by_an_interrupted_adoption() {
    use std::os::unix::fs::{symlink, MetadataExt};

    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    write_skill(&project, ".yasm/skills/demo", "demo", "Demo", "same");
    let record = serde_json::json!({
        "name": "Demo",
        "source": { "kind": "owned", "path": "." },
        "skill_path": "SKILL.md",
        "digest": "recorded",
        "enabled": []
    });
    let original_lock = serde_json::json!({
        "version": 3,
        "skills": { "demo": record }
    });
    std::fs::write(
        project.join(".yasm/yasm.lock"),
        serde_json::to_vec_pretty(&original_lock).unwrap(),
    )
    .unwrap();
    let source = project.join(".agents/skills/demo");
    write_skill(&project, ".agents/skills/demo", "demo", "Demo", "same");
    let backup = project.join(".agents/skills/.yasm-migrate-backup-adoption/skill");
    std::fs::create_dir_all(backup.parent().unwrap()).unwrap();
    std::fs::rename(&source, &backup).unwrap();
    symlink("../../.yasm/skills/demo", &source).unwrap();
    let store = project.join(".yasm/skills/demo");
    let store_inode = std::fs::metadata(&store).unwrap().ino();
    let journal = serde_json::json!({
        "initializing": false,
        "lock_existed": true,
        "original_lock": original_lock,
        "changes": [{
            "skill_id": "demo",
            "locations": [{
                "source": source,
                "backup": backup,
                "expected_target": "../../.yasm/skills/demo",
                "original_was_symlink": false,
                "resolved_parent": source.parent().unwrap().canonicalize().unwrap()
            }],
            "store_created": false
        }]
    });
    std::fs::write(
        project.join(".yasm/migration-journal.json"),
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "review"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("recovered an interrupted migration"));
    assert_eq!(std::fs::metadata(&store).unwrap().ino(), store_inode);
    assert!(source.join("SKILL.md").exists());
    assert!(!std::fs::symlink_metadata(&source)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!project.join(".yasm/migration-journal.json").exists());
}

#[test]
fn migrate_recovery_rejects_store_ownership_that_disagrees_with_original_lock() {
    for (store_created, original_had_skill) in [(true, true), (false, false)] {
        let mut command = yasm();
        let project = command.get_current_dir().unwrap().to_path_buf();
        write_skill(&project, ".yasm/skills/demo", "demo", "Demo", "keep");
        let skills = if original_had_skill {
            serde_json::json!({
                "demo": {
                    "name": "Demo",
                    "source": { "kind": "owned", "path": "." },
                    "skill_path": "SKILL.md",
                    "digest": "recorded",
                    "enabled": []
                }
            })
        } else {
            serde_json::json!({})
        };
        let original_lock = serde_json::json!({ "version": 3, "skills": skills });
        std::fs::write(
            project.join(".yasm/yasm.lock"),
            serde_json::to_vec_pretty(&original_lock).unwrap(),
        )
        .unwrap();
        let journal = serde_json::json!({
            "initializing": false,
            "lock_existed": true,
            "original_lock": original_lock,
            "changes": [{
                "skill_id": "demo",
                "locations": [],
                "store_created": store_created
            }]
        });
        std::fs::write(
            project.join(".yasm/migration-journal.json"),
            serde_json::to_vec_pretty(&journal).unwrap(),
        )
        .unwrap();

        let output = command
            .args(["migrate", "--action", "review"])
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("store ownership"));
        assert!(project.join(".yasm/skills/demo/SKILL.md").exists());
        assert!(project.join(".yasm/migration-journal.json").exists());
    }
}

#[cfg(unix)]
#[test]
fn migrate_recovery_restores_a_journaled_agent_alias() {
    use std::os::unix::fs::symlink;

    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    let canonical = project.join(".agents/skills/demo");
    let alias = project.join(".claude/skills/demo");
    let canonical_backup = project.join(".agents/skills/.yasm-migrate-backup-canonical/skill");
    let alias_backup = project.join(".claude/skills/.yasm-migrate-backup-alias/skill");
    write_skill(
        &project,
        ".agents/skills/.yasm-migrate-backup-canonical/skill",
        "demo",
        "Demo",
        "original",
    );
    std::fs::create_dir_all(alias_backup.parent().unwrap()).unwrap();
    symlink("../../.agents/skills/demo", &alias_backup).unwrap();
    std::fs::create_dir_all(project.join(".yasm/skills/demo")).unwrap();
    std::fs::copy(
        canonical_backup.join("SKILL.md"),
        project.join(".yasm/skills/demo/SKILL.md"),
    )
    .unwrap();
    symlink("../../.yasm/skills/demo", &canonical).unwrap();
    symlink("../../.yasm/skills/demo", &alias).unwrap();
    let journal = serde_json::json!({
        "initializing": false,
        "lock_existed": false,
        "original_lock": { "version": 3, "skills": {} },
        "changes": [{
            "skill_id": "demo",
            "store_created": true,
            "locations": [
                {
                    "source": canonical,
                    "backup": canonical_backup,
                    "expected_target": "../../.yasm/skills/demo",
                    "original_was_symlink": false,
                    "resolved_parent": canonical.parent().unwrap().canonicalize().unwrap()
                },
                {
                    "source": alias,
                    "backup": alias_backup,
                    "expected_target": "../../.yasm/skills/demo",
                    "original_was_symlink": true,
                    "resolved_parent": alias.parent().unwrap().canonicalize().unwrap()
                }
            ]
        }]
    });
    std::fs::write(
        project.join(".yasm/migration-journal.json"),
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "review"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "recovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(canonical.join("SKILL.md").exists());
    assert_eq!(
        std::fs::read_link(&alias).unwrap(),
        std::path::PathBuf::from("../../.agents/skills/demo")
    );
    assert!(!project.join(".yasm/skills/demo").exists());
    assert!(!project.join(".yasm/migration-journal.json").exists());
}

#[test]
fn migrate_rejects_journal_paths_outside_registered_agent_directories() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    std::fs::create_dir(project.join(".yasm")).unwrap();
    let external = tempdir().unwrap();
    let source = external.path().join("source");
    let backup = external.path().join(".yasm-migrate-backup-forged/skill");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(source.join("important.txt"), "keep").unwrap();
    let journal = serde_json::json!({
        "initializing": false,
        "lock_existed": false,
        "original_lock": { "version": 3, "skills": {} },
        "changes": [{
            "skill_id": "demo",
            "store_created": true,
            "locations": [{
                "source": source,
                "backup": backup,
                "expected_target": "../../.yasm/skills/demo",
                "original_was_symlink": false,
                "resolved_parent": source.parent().unwrap().canonicalize().unwrap()
            }]
        }]
    });
    std::fs::write(
        project.join(".yasm/migration-journal.json"),
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "review"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a registered agent path"));
    assert!(external.path().join("source/important.txt").exists());
    assert!(external
        .path()
        .join(".yasm-migrate-backup-forged/skill")
        .exists());
}

#[cfg(unix)]
#[test]
fn recovery_preserves_store_when_backup_is_missing() {
    use std::os::unix::fs::symlink;

    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    let source = project.join(".agents/skills/demo");
    let backup = project.join(".agents/skills/.yasm-migrate-backup-missing/skill");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::create_dir_all(project.join(".yasm/skills/demo")).unwrap();
    std::fs::write(project.join(".yasm/skills/demo/SKILL.md"), "only copy").unwrap();
    symlink("../../.yasm/skills/demo", &source).unwrap();
    let journal = serde_json::json!({
        "initializing": false,
        "lock_existed": false,
        "original_lock": { "version": 3, "skills": {} },
        "changes": [{
            "skill_id": "demo",
            "store_created": true,
            "locations": [{
                "source": source,
                "backup": backup,
                "expected_target": "../../.yasm/skills/demo",
                "original_was_symlink": false,
                "resolved_parent": source.parent().unwrap().canonicalize().unwrap()
            }]
        }]
    });
    std::fs::write(
        project.join(".yasm/migration-journal.json"),
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();

    let output = command
        .args(["migrate", "--action", "review"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("backup"));
    assert!(project.join(".yasm/skills/demo/SKILL.md").exists());
    assert!(std::fs::symlink_metadata(&source)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(project.join(".yasm/migration-journal.json").exists());
}

#[test]
fn repeated_init_does_not_process_an_existing_migration_journal() {
    let mut command = yasm();
    let project = command.get_current_dir().unwrap().to_path_buf();
    std::fs::create_dir(project.join(".yasm")).unwrap();
    std::fs::write(
        project.join(".yasm/migration-journal.json"),
        "not valid json",
    )
    .unwrap();

    let output = command.args(["init", "--no-migrate"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Yasm already exists"));
    assert_eq!(
        std::fs::read_to_string(project.join(".yasm/migration-journal.json")).unwrap(),
        "not valid json"
    );
}

#[cfg(unix)]
fn copy_dir(source: &std::path::Path, destination: &std::path::Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            copy_dir(&from, &to);
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&from).unwrap();
            std::os::unix::fs::symlink(target, &to).unwrap();
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

#[test]
fn incomplete_migration_journals_preserve_backups_and_store() {
    for field in ["store_created", "original_was_symlink", "resolved_parent"] {
        let mut command = yasm();
        let project = command.get_current_dir().unwrap().to_path_buf();
        let backup = project.join(".agents/skills/.yasm-migrate-backup-test/skill");
        write_skill(
            &project,
            ".agents/skills/.yasm-migrate-backup-test/skill",
            "demo",
            "Demo",
            "backup",
        );
        write_skill(&project, ".yasm/skills/demo", "demo", "Demo", "store");
        let mut journal = serde_json::json!({
            "initializing": false, "lock_existed": false,
            "original_lock": { "version": 3, "skills": {} },
            "changes": [{
                "skill_id": "demo", "store_created": true,
                "locations": [{
                    "source": project.join(".agents/skills/demo"),
                    "backup": backup,
                    "expected_target": "../../.yasm/skills/demo",
                    "original_was_symlink": false,
                    "resolved_parent": project.join(".agents/skills").canonicalize().unwrap()
                }]
            }]
        });
        let change = &mut journal["changes"][0];
        if field == "store_created" {
            change.as_object_mut().unwrap().remove(field);
        } else {
            change["locations"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
        }
        let path = project.join(".yasm/migration-journal.json");
        let bytes = serde_json::to_vec_pretty(&journal).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let output = command
            .args(["migrate", "--action", "review"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("missing field `{field}`"))
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(backup.join("SKILL.md").exists());
        assert!(project.join(".yasm/skills/demo/SKILL.md").exists());
    }
}

fn agent_files_command(project: &Path, home: &Path, data: &Path) -> TestCommand {
    let mut command = yasm_with_roots(data, home);
    command.current_dir(project).env_remove("CODEX_HOME");
    command
}

#[cfg(unix)]
#[test]
fn agent_files_migration_preserves_root_files_and_leaves_skills_and_nested_files_alone() {
    let project = tempdir().unwrap();
    let home = tempdir().unwrap();
    let data = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::create_dir_all(project.path().join(".agents/skills/demo")).unwrap();
    std::fs::write(project.path().join("AGENTS.md"), b"global\r\n\xff").unwrap();
    std::fs::write(project.path().join("CLAUDE.md"), "claude instructions\n").unwrap();
    std::fs::write(project.path().join("src/AGENTS.md"), "nested").unwrap();
    std::fs::write(
        project.path().join(".agents/skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: demo\n---\n",
    )
    .unwrap();
    for _ in 0..2 {
        let output = agent_files_command(project.path(), home.path(), data.path())
            .args(["migrate", "--agent-files", "--action", "apply"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let original = project.path().join(name);
        assert_eq!(
            std::fs::read_link(&original).unwrap(),
            Path::new(".yasm/agent-files").join(name)
        );
        assert_eq!(
            std::fs::read(original).unwrap(),
            std::fs::read(project.path().join(".yasm/agent-files").join(name)).unwrap()
        );
    }
    assert_eq!(
        std::fs::read(project.path().join("AGENTS.md")).unwrap(),
        b"global\r\n\xff"
    );
    std::fs::write(project.path().join("AGENTS.md"), "edited").unwrap();
    assert_eq!(
        std::fs::read_to_string(project.path().join(".yasm/agent-files/AGENTS.md")).unwrap(),
        "edited"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("src/AGENTS.md")).unwrap(),
        "nested"
    );
    assert!(
        std::fs::symlink_metadata(project.path().join(".agents/skills/demo"))
            .unwrap()
            .is_dir()
    );
    assert!(!project
        .path()
        .join(".yasm/agent-file-migration.json")
        .exists());
    assert!(!project.path().join(".yasm/yasm.lock").exists());
}

#[test]
fn agent_files_review_and_missing_action_make_no_changes_and_show_conflicts() {
    let project = tempdir().unwrap();
    let home = tempdir().unwrap();
    let data = tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".yasm/agent-files")).unwrap();
    std::fs::write(project.path().join("AGENTS.md"), "original").unwrap();
    std::fs::write(project.path().join("CLAUDE.md"), "claude").unwrap();
    std::fs::write(
        project.path().join(".yasm/agent-files/AGENTS.md"),
        "different",
    )
    .unwrap();
    let review = agent_files_command(project.path(), home.path(), data.path())
        .args(["migrate", "--agent-files", "--action", "review"])
        .output()
        .unwrap();
    assert!(review.status.success());
    let stdout = String::from_utf8_lossy(&review.stdout);
    assert!(stdout.contains("CONFLICT"));
    assert!(stdout.contains("relative symlink"));
    for args in [
        vec!["migrate", "--agent-files"],
        vec!["migrate", "--agent-files", "--action", "apply"],
    ] {
        let output = agent_files_command(project.path(), home.path(), data.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--action"));
    }
    assert_eq!(
        std::fs::read_to_string(project.path().join("AGENTS.md")).unwrap(),
        "original"
    );
    assert!(std::fs::symlink_metadata(project.path().join("CLAUDE.md"))
        .unwrap()
        .is_file());
    assert!(!project.path().join(".yasm/agent-files/CLAUDE.md").exists());
    assert!(!project
        .path()
        .join(".yasm/agent-file-migration.json")
        .exists());
}

#[cfg(unix)]
#[test]
fn agent_files_adopts_identical_store_but_rejects_external_and_broken_symlinks() {
    let project = tempdir().unwrap();
    let home = tempdir().unwrap();
    let data = tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".yasm/agent-files")).unwrap();
    std::fs::write(project.path().join("AGENTS.md"), "same").unwrap();
    std::fs::write(project.path().join(".yasm/agent-files/AGENTS.md"), "same").unwrap();
    let apply = agent_files_command(project.path(), home.path(), data.path())
        .args(["migrate", "--agent-files", "--action", "apply"])
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "{}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let external = project.path().join("external.md");
    std::fs::write(&external, "external").unwrap();
    for target in [external, project.path().join("missing.md")] {
        let claude = project.path().join("CLAUDE.md");
        std::os::unix::fs::symlink(&target, &claude).unwrap();
        let apply = agent_files_command(project.path(), home.path(), data.path())
            .args(["migrate", "--agent-files", "--action", "apply"])
            .output()
            .unwrap();
        assert!(!apply.status.success());
        assert!(String::from_utf8_lossy(&apply.stderr).contains("symlink"));
        assert_eq!(std::fs::read_link(&claude).unwrap(), target);
        std::fs::remove_file(claude).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(project.path().join("external.md")).unwrap(),
        "external"
    );
    let store = project.path().join(".yasm/agent-files/AGENTS.md");
    std::fs::remove_file(store).unwrap();
    let broken = agent_files_command(project.path(), home.path(), data.path())
        .args(["migrate", "--agent-files", "--action", "apply"])
        .output()
        .unwrap();
    assert!(!broken.status.success());
    assert!(std::fs::symlink_metadata(project.path().join("AGENTS.md"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(unix)]
#[test]
fn global_agent_files_respect_codex_home_and_symlinked_configuration_parents() {
    let project = tempdir().unwrap();
    let home = tempdir().unwrap();
    let data = tempdir().unwrap();
    let external = tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();
    std::fs::write(home.path().join(".codex/AGENTS.md"), "default ignored").unwrap();
    let codex = external.path().join("profile");
    std::fs::create_dir(&codex).unwrap();
    std::fs::write(codex.join("AGENTS.md"), "codex").unwrap();
    std::fs::write(codex.join("AGENTS.override.md"), "override").unwrap();
    let physical_claude = external.path().join("configuration/claude");
    std::fs::create_dir_all(&physical_claude).unwrap();
    std::fs::write(physical_claude.join("CLAUDE.md"), "claude").unwrap();
    std::os::unix::fs::symlink(&physical_claude, home.path().join(".claude")).unwrap();
    for action in ["review", "apply", "apply"] {
        let output = agent_files_command(project.path(), home.path(), data.path())
            .env("CODEX_HOME", &codex)
            .args(["migrate", "--agent-files", "--global", "--action", action])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("AGENTS.override.md"));
        if action == "review" {
            assert!(!data.path().join("agent-files").exists());
        }
    }
    for source in [
        codex.join("AGENTS.md"),
        home.path().join(".claude/CLAUDE.md"),
    ] {
        assert!(std::fs::read_link(&source).unwrap().is_relative());
        assert_eq!(
            std::fs::canonicalize(&source).unwrap(),
            std::fs::canonicalize(
                data.path()
                    .join("agent-files")
                    .join(source.file_name().unwrap())
            )
            .unwrap()
        );
    }
    assert_eq!(
        std::fs::read_link(home.path().join(".claude")).unwrap(),
        physical_claude
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join(".codex/AGENTS.md")).unwrap(),
        "default ignored"
    );
    assert_eq!(
        std::fs::read_to_string(codex.join("AGENTS.override.md")).unwrap(),
        "override"
    );
}

#[test]
fn global_agent_files_discover_defaults_and_skip_missing_files_without_creating_store() {
    let project = tempdir().unwrap();
    let home = tempdir().unwrap();
    let data = tempdir().unwrap();
    let empty = agent_files_command(project.path(), home.path(), data.path())
        .args(["migrate", "--agent-files", "--global", "--action", "apply"])
        .output()
        .unwrap();
    assert!(empty.status.success());
    assert!(!data.path().join("agent-files").exists());
    std::fs::create_dir(home.path().join(".codex")).unwrap();
    std::fs::write(home.path().join(".codex/AGENTS.md"), "default").unwrap();
    let review = agent_files_command(project.path(), home.path(), data.path())
        .args(["migrate", "--agent-files", "--global", "--action", "review"])
        .output()
        .unwrap();
    assert!(review.status.success());
    assert!(String::from_utf8_lossy(&review.stdout).contains("AGENTS.md"));
    assert!(!data.path().join("agent-files").exists());
}

#[test]
fn agent_files_flag_rejects_skill_specific_options() {
    for option in [
        vec!["--skill", "demo"],
        vec!["--source", "local"],
        vec!["--with-upstream"],
    ] {
        let output = yasm()
            .args(["migrate", "--agent-files", "--action", "review"])
            .args(option)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    }
}

#[cfg(unix)]
#[test]
fn interactive_agent_files_migration_offers_review_and_apply() {
    let project = tempdir().unwrap();
    let home = tempdir().unwrap();
    let data = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    std::fs::write(project.path().join("AGENTS.md"), "instructions").unwrap();
    for apply in [false, true] {
        let mut command = agent_files_command(project.path(), home.path(), data.path());
        command.args(["migrate", "--agent-files"]);
        let TestCommand { command, _sandbox } = command;
        let mut session = Session::spawn(command).unwrap();
        session
            .expect("Choose an agent-file migration action")
            .unwrap();
        if apply {
            session.send("\x1b[B").unwrap();
        }
        session.send_line("").unwrap();
        session.expect(Eof).unwrap();
        let status = session.get_process().wait().unwrap();
        assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
        assert_eq!(
            std::fs::symlink_metadata(project.path().join("AGENTS.md"))
                .unwrap()
                .file_type()
                .is_symlink(),
            apply
        );
    }
}

#[test]
fn skill_migration_does_not_adopt_agent_files_without_flag() {
    let project = tempdir().unwrap();
    let home = tempdir().unwrap();
    let data = tempdir().unwrap();
    std::fs::create_dir(project.path().join(".yasm")).unwrap();
    std::fs::write(project.path().join("AGENTS.md"), "instructions").unwrap();
    let output = agent_files_command(project.path(), home.path(), data.path())
        .args(["migrate", "--action", "apply"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(std::fs::symlink_metadata(project.path().join("AGENTS.md"))
        .unwrap()
        .is_file());
    assert!(!project.path().join(".yasm/agent-files").exists());
}

fn repeated_add_repository() -> TempDir {
    let remote = tempdir().unwrap();
    git(remote.path(), &["init", "-b", "main"]);
    git(
        remote.path(),
        &["config", "user.email", "test@example.test"],
    );
    git(remote.path(), &["config", "user.name", "Test"]);
    for name in ["retro", "research"] {
        write_skill(
            remote.path(),
            &format!("skills/{name}"),
            name,
            name,
            "original",
        );
    }
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "initial"]);
    remote
}

fn repeated_add_command(data: &Path, agents: &Path, remote: &Path) -> TestCommand {
    let mut command = yasm_with_roots(data, agents);
    redirect_github(&mut command, remote);
    command
}

fn installation_receipts(data: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(data.join("yasm.lock")).unwrap()).unwrap()
}

#[test]
fn repeated_add_recognizes_equivalent_sources_and_preserves_sibling_receipts() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let remote = repeated_add_repository();
    let installed = repeated_add_command(data.path(), agents.path(), remote.path())
        .args([
            "add",
            "owner/repo",
            "--skill",
            "retro",
            "--agent",
            "claude",
            "--agent",
            "universal",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let original = installation_receipts(data.path())["skills"]["retro"].clone();
    write_skill(
        remote.path(),
        "skills/retro",
        "retro",
        "retro",
        "upstream changed",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "advance"]);

    let tree = repeated_add_command(data.path(), agents.path(), remote.path())
        .args([
            "add",
            "https://github.com/OWNER/REPO.git/tree/main/skills/retro",
        ])
        .output()
        .unwrap();
    assert!(
        tree.status.success(),
        "{}",
        String::from_utf8_lossy(&tree.stderr)
    );
    assert!(String::from_utf8_lossy(&tree.stdout).contains("✔ retro"));
    assert_eq!(
        installation_receipts(data.path())["skills"]["retro"],
        original
    );

    let missing_selection = repeated_add_command(data.path(), agents.path(), remote.path())
        .args(["add", "https://github.com/owner/repo/"])
        .output()
        .unwrap();
    assert!(!missing_selection.status.success());
    assert!(String::from_utf8_lossy(&missing_selection.stdout).contains("✔ retro"));
    assert!(String::from_utf8_lossy(&missing_selection.stderr).contains("--skill <name>"));
    assert_eq!(
        installation_receipts(data.path())["skills"]["retro"],
        original
    );

    let added = repeated_add_command(data.path(), agents.path(), remote.path())
        .args([
            "add",
            "owner/repo",
            "--skill",
            "research",
            "--agent",
            "claude",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert_eq!(
        installation_receipts(data.path())["skills"]["retro"],
        original
    );
    let before = std::fs::read(data.path().join("yasm.lock")).unwrap();
    let all = repeated_add_command(data.path(), agents.path(), remote.path())
        .args(["add", "owner/repo"])
        .output()
        .unwrap();
    assert!(all.status.success());
    let output = String::from_utf8_lossy(&all.stdout);
    assert!(output.contains("✔ retro") && output.contains("✔ research"));
    assert_eq!(
        std::fs::read(data.path().join("yasm.lock")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_dir(data.path().join("cache/sources/repositories"))
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
            .count(),
        1
    );

    // Removing upstream skills and deleting caches must not remove installations.
    git(remote.path(), &["rm", "-r", "skills/retro"]);
    git(remote.path(), &["commit", "-m", "retire retro"]);
    std::fs::remove_dir_all(data.path().join("cache")).unwrap();
    let repeated = repeated_add_command(data.path(), agents.path(), remote.path())
        .args(["add", "owner/repo"])
        .output()
        .unwrap();
    assert!(repeated.status.success());
    assert_eq!(
        std::fs::read(data.path().join("yasm.lock")).unwrap(),
        before
    );
    assert!(
        std::fs::read_to_string(data.path().join("skills/retro/SKILL.md"))
            .unwrap()
            .contains("original")
    );
}

#[cfg(unix)]
#[test]
fn repeated_add_picker_only_offers_additional_skills() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let remote = repeated_add_repository();
    assert!(
        repeated_add_command(data.path(), agents.path(), remote.path())
            .args([
                "add",
                "owner/repo",
                "--skill",
                "retro",
                "--no-enable",
                "--action",
                "apply"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    let before = installation_receipts(data.path())["skills"]["retro"].clone();
    let mut command = repeated_add_command(data.path(), agents.path(), remote.path());
    command.args(["add", "owner/repo", "--no-enable", "--action", "apply"]);
    let TestCommand { command, _sandbox } = command;
    let mut session = Session::spawn(command).unwrap();
    session
        .expect("Already installed from this source:")
        .unwrap();
    session.expect("✔ retro").unwrap();
    session
        .expect("Select additional skills to install")
        .unwrap();
    session.expect("research").unwrap();
    session.send(" ").unwrap();
    session.send_line("").unwrap();
    session.expect("acquired research").unwrap();
    session.expect(Eof).unwrap();
    assert!(matches!(
        session.get_process().wait().unwrap(),
        WaitStatus::Exited(_, 0)
    ));
    assert_eq!(
        installation_receipts(data.path())["skills"]["retro"],
        before
    );
    assert!(data.path().join("skills/research/SKILL.md").is_file());
}

#[test]
fn different_upstream_requires_replace_even_with_identical_contents() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let first = tempdir().unwrap();
    let second = tempdir().unwrap();
    for source in [&first, &second] {
        write_skill(source.path(), "skills/retro", "retro", "retro", "same");
    }
    assert!(yasm_with_roots(data.path(), agents.path())
        .args(["add", "--no-enable", "--action", "apply"])
        .arg(first.path())
        .output()
        .unwrap()
        .status
        .success());
    let before = std::fs::read(data.path().join("yasm.lock")).unwrap();
    let conflict = yasm_with_roots(data.path(), agents.path())
        .args([
            "add",
            "--skill",
            "retro",
            "--no-enable",
            "--action",
            "apply",
        ])
        .arg(second.path())
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("--replace"));
    assert_eq!(
        std::fs::read(data.path().join("yasm.lock")).unwrap(),
        before
    );
    let replaced = yasm_with_roots(data.path(), agents.path())
        .args([
            "add",
            "--skill",
            "retro",
            "--replace",
            "--no-enable",
            "--action",
            "apply",
        ])
        .arg(second.path())
        .output()
        .unwrap();
    assert!(
        replaced.status.success(),
        "{}",
        String::from_utf8_lossy(&replaced.stderr)
    );
    assert_eq!(
        installation_receipts(data.path())["skills"]["retro"]["source"]["path"],
        second.path().canonicalize().unwrap().to_str().unwrap()
    );
}

#[cfg(unix)]
#[test]
fn explicit_add_refreshes_revision_and_repairs_files_and_retained_links() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let remote = repeated_add_repository();
    for name in ["retro", "research"] {
        assert!(
            repeated_add_command(data.path(), agents.path(), remote.path())
                .args([
                    "add",
                    "owner/repo",
                    "--skill",
                    name,
                    "--agent",
                    "claude",
                    "--action",
                    "apply"
                ])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let before = installation_receipts(data.path());
    write_skill(
        remote.path(),
        "skills/research",
        "research",
        "research",
        "updated research",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "update sibling"]);
    let refreshed = repeated_add_command(data.path(), agents.path(), remote.path())
        .args([
            "add",
            "owner/repo",
            "--skill",
            "retro",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(refreshed.status.success());
    let receipts = installation_receipts(data.path());
    assert_ne!(
        receipts["skills"]["retro"]["resolved"],
        before["skills"]["retro"]["resolved"]
    );
    assert_eq!(receipts["skills"]["research"], before["skills"]["research"]);
    let link = agents.path().join(".claude/skills/retro");
    std::fs::remove_dir_all(data.path().join("skills/retro")).unwrap();
    std::fs::remove_file(&link).unwrap();
    let repaired = repeated_add_command(data.path(), agents.path(), remote.path())
        .args([
            "add",
            "owner/repo",
            "--skill",
            "retro",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert!(link.join("SKILL.md").is_file());
    assert_eq!(
        installation_receipts(data.path())["skills"]["retro"]["enabled"],
        serde_json::json!(["claude"])
    );
    assert_eq!(
        installation_receipts(data.path())["skills"]["research"],
        before["skills"]["research"]
    );
    // A manually synchronized copy needs a fresh digest even without a content diff.
    let previous_digest = installation_receipts(data.path())["skills"]["retro"]["digest"].clone();
    write_skill(
        remote.path(),
        "skills/retro",
        "retro",
        "retro",
        "manually synchronized",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "synchronize retro"]);
    std::fs::copy(
        remote.path().join("skills/retro/SKILL.md"),
        link.join("SKILL.md"),
    )
    .unwrap();
    let synchronized = repeated_add_command(data.path(), agents.path(), remote.path())
        .args([
            "add",
            "owner/repo",
            "--skill",
            "retro",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(synchronized.status.success());
    assert_ne!(
        installation_receipts(data.path())["skills"]["retro"]["digest"],
        previous_digest
    );
    assert_eq!(
        installation_receipts(data.path())["skills"]["research"],
        before["skills"]["research"]
    );
    // Skipping an explicit refresh preserves local modifications and receipts.
    std::fs::write(link.join("SKILL.md"), "local changes").unwrap();
    let receipt = std::fs::read(data.path().join("yasm.lock")).unwrap();
    assert!(
        repeated_add_command(data.path(), agents.path(), remote.path())
            .args([
                "add",
                "owner/repo",
                "--skill",
                "retro",
                "--no-enable",
                "--action",
                "skip"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        std::fs::read_to_string(link.join("SKILL.md")).unwrap(),
        "local changes"
    );
    assert_eq!(
        std::fs::read(data.path().join("yasm.lock")).unwrap(),
        receipt
    );
}

#[test]
fn repository_cache_is_shared_between_project_and_global_installations() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let project = tempdir().unwrap();
    let remote = repeated_add_repository();
    assert!(
        repeated_add_command(data.path(), agents.path(), remote.path())
            .args([
                "add",
                "owner/repo",
                "--global",
                "--skill",
                "retro",
                "--no-enable",
                "--action",
                "apply"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(yasm_with_roots(data.path(), agents.path())
        .current_dir(project.path())
        .args(["init", "--no-migrate"])
        .output()
        .unwrap()
        .status
        .success());
    let original = std::fs::read(data.path().join("yasm.lock")).unwrap();
    let added = repeated_add_command(data.path(), agents.path(), remote.path())
        .current_dir(project.path())
        .args([
            "add",
            "owner/repo",
            "--skill",
            "retro",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(project.path().join(".yasm/skills/retro/SKILL.md").is_file());
    assert_eq!(
        std::fs::read(data.path().join("yasm.lock")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read_dir(data.path().join("cache/sources/repositories"))
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
            .count(),
        1
    );
}

#[test]
fn upstream_identity_distinguishes_refs_and_paths_but_tracks_default_branch_changes() {
    let data = tempdir().unwrap();
    let agents = tempdir().unwrap();
    let remote = repeated_add_repository();
    assert!(
        repeated_add_command(data.path(), agents.path(), remote.path())
            .args([
                "add",
                "owner/repo",
                "--skill",
                "retro",
                "--no-enable",
                "--action",
                "apply"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    let original = std::fs::read(data.path().join("yasm.lock")).unwrap();
    git(remote.path(), &["checkout", "-b", "other"]);
    write_skill(
        remote.path(),
        "other/retro",
        "retro",
        "retro",
        "same name, other path",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "other branch"]);
    let other_ref = repeated_add_command(data.path(), agents.path(), remote.path())
        .args([
            "add",
            "owner/repo@other",
            "--skill",
            "retro",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(!other_ref.status.success());
    assert!(String::from_utf8_lossy(&other_ref.stderr).contains("ambiguous"));
    let other_ref_path = repeated_add_command(data.path(), agents.path(), remote.path())
        .args([
            "add",
            "owner/repo@other",
            "--skill",
            "skills/retro/SKILL.md",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(!other_ref_path.status.success());
    assert!(String::from_utf8_lossy(&other_ref_path.stderr).contains("--replace"));
    assert_eq!(
        std::fs::read(data.path().join("yasm.lock")).unwrap(),
        original
    );
    // A source filter isolates the same name at a different path on main.
    git(remote.path(), &["checkout", "main"]);
    write_skill(
        remote.path(),
        "other/retro",
        "retro",
        "retro",
        "same name, other path",
    );
    git(remote.path(), &["add", "."]);
    git(remote.path(), &["commit", "-m", "duplicate path"]);
    let other_path = repeated_add_command(data.path(), agents.path(), remote.path())
        .args([
            "add",
            "https://github.com/owner/repo/tree/main/other/retro",
            "--no-enable",
            "--action",
            "apply",
        ])
        .output()
        .unwrap();
    assert!(!other_path.status.success());
    assert!(String::from_utf8_lossy(&other_path.stderr).contains("--replace"));
    assert_eq!(
        std::fs::read(data.path().join("yasm.lock")).unwrap(),
        original
    );

    // Rename the default branch without changing which skill was requested.
    git(remote.path(), &["branch", "-m", "primary"]);
    let tracked_default = repeated_add_command(data.path(), agents.path(), remote.path())
        .args(["add", "owner/repo"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&tracked_default.stdout).contains("✔ retro"));
    assert_eq!(
        std::fs::read(data.path().join("yasm.lock")).unwrap(),
        original
    );
}

#[cfg(unix)]
#[test]
fn explicit_add_preflights_retained_agent_conflicts_before_changing_installation() {
    for foreign_symlink in [false, true] {
        let data = tempdir().unwrap();
        let agents = tempdir().unwrap();
        let remote = repeated_add_repository();
        assert!(
            repeated_add_command(data.path(), agents.path(), remote.path())
                .args([
                    "add",
                    "owner/repo",
                    "--skill",
                    "retro",
                    "--agent",
                    "claude",
                    "--action",
                    "apply"
                ])
                .output()
                .unwrap()
                .status
                .success()
        );
        let link = agents.path().join(".claude/skills/retro");
        std::fs::remove_file(&link).unwrap();
        if foreign_symlink {
            let foreign = agents.path().join("foreign");
            std::fs::create_dir(&foreign).unwrap();
            std::os::unix::fs::symlink(&foreign, &link).unwrap();
        } else {
            std::fs::create_dir(&link).unwrap();
            std::fs::write(link.join("local.txt"), "preserve this directory").unwrap();
        }
        write_skill(
            remote.path(),
            "skills/retro",
            "retro",
            "retro",
            "new upstream contents",
        );
        git(remote.path(), &["add", "."]);
        git(remote.path(), &["commit", "-m", "change retro"]);
        let before = std::fs::read(data.path().join("yasm.lock")).unwrap();
        let contents = std::fs::read(data.path().join("skills/retro/SKILL.md")).unwrap();
        let failed = repeated_add_command(data.path(), agents.path(), remote.path())
            .args([
                "add",
                "owner/repo",
                "--skill",
                "retro",
                "--no-enable",
                "--action",
                "apply",
            ])
            .output()
            .unwrap();
        assert!(!failed.status.success());
        assert_eq!(
            std::fs::read(data.path().join("yasm.lock")).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read(data.path().join("skills/retro/SKILL.md")).unwrap(),
            contents
        );
        if !foreign_symlink {
            assert!(String::from_utf8_lossy(&failed.stderr).contains("--replace"));
            let replaced = repeated_add_command(data.path(), agents.path(), remote.path())
                .args([
                    "add",
                    "owner/repo",
                    "--skill",
                    "retro",
                    "--no-enable",
                    "--replace",
                    "--action",
                    "apply",
                ])
                .output()
                .unwrap();
            assert!(
                replaced.status.success(),
                "{}",
                String::from_utf8_lossy(&replaced.stderr)
            );
            assert!(std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert!(std::fs::read_to_string(link.join("SKILL.md"))
                .unwrap()
                .contains("new upstream contents"));
            assert_eq!(
                installation_receipts(data.path())["skills"]["retro"]["enabled"],
                serde_json::json!(["claude"])
            );
        }
    }
}

#[test]
fn repeated_add_supports_relative_cache_directories() {
    let remote = repeated_add_repository();
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let output = repeated_add_command(data.path(), agents.path(), remote.path())
            .current_dir(workspace.path())
            .env("YASM_CACHE_DIR", "relative-cache")
            .args([
                "add",
                "owner/repo",
                "--skill",
                "retro",
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
    }
    assert_eq!(
        std::fs::read_dir(workspace.path().join("relative-cache/sources/repositories"))
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
            .count(),
        1
    );
}

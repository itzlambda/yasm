#![cfg(feature = "marketplace")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

struct Sandbox {
    root: TempDir,
    home: PathBuf,
    agents: PathBuf,
    data: PathBuf,
    cache: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let agents = root.path().join("agents");
        let data = root.path().join("data");
        let cache = root.path().join("cache");
        let config = root.path().join("config");
        let workspace = root.path().join("workspace");
        for path in [&home, &agents, &workspace] {
            std::fs::create_dir_all(path).unwrap();
        }
        Self {
            root,
            home,
            agents,
            data,
            cache,
            config,
            workspace,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_yasm"));
        command
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("YASM_DATA_DIR", &self.data)
            .env("YASM_CACHE_DIR", &self.cache)
            .env("YASM_CONFIG_DIR", &self.config)
            .env("YASM_AGENT_SKILLS_ROOT", &self.agents)
            .env_remove("CODEX_HOME");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn assert_success(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command {:?} failed:\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

#[test]
fn sandbox_never_inherits_the_callers_codex_home() {
    let sandbox = Sandbox::new();
    let command = sandbox.command();

    assert!(command
        .get_envs()
        .any(|(name, value)| name == "CODEX_HOME" && value.is_none()));
}

#[test]
fn three_input_formats_export_skills_and_mcp_to_three_targets() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.home.join(".codex")).unwrap();
    std::fs::write(
        sandbox.home.join(".codex/config.toml"),
        "model = \"demo\"\n\n[mcp_servers.user]\ncommand = \"user-server\"\n",
    )
    .unwrap();
    std::fs::write(
        sandbox.home.join(".claude.json"),
        r#"{"theme":"dark","mcpServers":{"user":{"command":"user-server"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(sandbox.home.join(".cursor")).unwrap();
    std::fs::write(
        sandbox.home.join(".cursor/mcp.json"),
        r#"{"theme":"dark","mcpServers":{"user":{"command":"user-server"}}}"#,
    )
    .unwrap();

    for format in ["codex", "claude", "cursor"] {
        let source = create_marketplace(sandbox.root.path(), format);
        let source = source.to_str().unwrap();
        sandbox.assert_success(&[
            "marketplace",
            "add",
            source,
            "--alias",
            format,
            "--format",
            format,
            "--global",
        ]);
        let plugin = format!("plugin-{format}@{format}");
        sandbox.assert_success(&["plugin", "add", &plugin, "--no-enable", "--global"]);
        for target in ["codex", "claude", "cursor"] {
            sandbox.assert_success(&["plugin", "enable", &plugin, "--agent", target, "--global"]);
        }
    }

    for format in ["codex", "claude", "cursor"] {
        let skill = format!("skill-{format}");
        assert!(sandbox
            .agents
            .join(".agents/skills")
            .join(&skill)
            .is_symlink());
        assert!(sandbox
            .agents
            .join(".claude/skills")
            .join(&skill)
            .is_symlink());
        assert!(sandbox
            .agents
            .join(".cursor/skills")
            .join(&skill)
            .is_symlink());
    }

    let codex = std::fs::read_to_string(sandbox.home.join(".codex/config.toml")).unwrap();
    assert!(codex.contains("model = \"demo\""));
    assert!(codex.contains("[mcp_servers.user]"));
    for format in ["codex", "claude", "cursor"] {
        assert!(codex.contains(&format!("[mcp_servers.stdio-{format}]")));
        assert!(codex.contains(&format!("[mcp_servers.http-{format}]")));
    }
    for path in [
        sandbox.home.join(".claude.json"),
        sandbox.home.join(".cursor/mcp.json"),
    ] {
        let config: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert!(config["mcpServers"]["user"].is_object());
        for format in ["codex", "claude", "cursor"] {
            assert!(config["mcpServers"][format!("stdio-{format}")].is_object());
            assert!(config["mcpServers"][format!("http-{format}")].is_object());
        }
    }

    let claude_path = sandbox.home.join(".claude.json");
    let mut claude: Value = serde_json::from_slice(&std::fs::read(&claude_path).unwrap()).unwrap();
    claude["mcpServers"]["stdio-codex"]["command"] = Value::String("locally-edited".into());
    std::fs::write(&claude_path, serde_json::to_vec_pretty(&claude).unwrap()).unwrap();
    let guarded = sandbox.run(&[
        "plugin",
        "disable",
        "plugin-codex@codex",
        "--agent",
        "claude",
        "--global",
    ]);
    assert!(!guarded.status.success());
    assert!(String::from_utf8_lossy(&guarded.stderr).contains("modified MCP entry"));
    assert!(sandbox
        .agents
        .join(".claude/skills/skill-codex")
        .is_symlink());
    let claude: Value = serde_json::from_slice(&std::fs::read(&claude_path).unwrap()).unwrap();
    assert_eq!(
        claude["mcpServers"]["stdio-codex"]["command"],
        "locally-edited"
    );

    sandbox.assert_success(&[
        "plugin",
        "disable",
        "plugin-codex@codex",
        "--agent",
        "codex",
        "--global",
    ]);
    let codex = std::fs::read_to_string(sandbox.home.join(".codex/config.toml")).unwrap();
    assert!(codex.contains("[mcp_servers.user]"));
    assert!(!codex.contains("[mcp_servers.stdio-codex]"));
    assert!(codex.contains("[mcp_servers.stdio-claude]"));
}

#[test]
fn non_tty_plugin_add_requires_agent_or_no_enable() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    let output = sandbox.run(&["plugin", "add", "plugin-claude", "--global"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--agent"));
}

#[test]
fn plugin_add_rejects_individual_component_selection() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "codex");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "codex",
        "--global",
    ]);

    let output = sandbox.run(&[
        "plugin",
        "add",
        "plugin-codex",
        "--agent",
        "codex",
        "--skill",
        "skill-codex",
        "--global",
    ]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--skill'"));
}

#[test]
fn plugin_enable_rejects_individual_component_selection() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "codex");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "codex",
        "--global",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-codex", "--no-enable", "--global"]);

    let output = sandbox.run(&[
        "plugin",
        "enable",
        "plugin-codex",
        "--agent",
        "codex",
        "--mcp-server",
        "stdio-codex",
        "--global",
    ]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--mcp-server'"));
}

#[test]
fn plugin_configure_command_is_not_exposed() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["plugin", "configure", "example", "--agent", "codex"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand 'configure'"));
}

#[test]
fn rejected_repeat_enable_does_not_leave_operation_journal() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "plugin-claude",
        "--agent",
        "codex",
        "--global",
    ]);

    let output = sandbox.run(&[
        "plugin",
        "enable",
        "plugin-claude",
        "--agent",
        "codex",
        "--global",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already enabled for codex"));
    assert_eq!(
        std::fs::read_dir(sandbox.data.join("marketplaces/operations"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn plugin_update_protects_and_can_explicitly_replace_snapshot_edits() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "cursor");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "cursor",
        "--global",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-cursor", "--no-enable", "--global"]);
    let state: Value = serde_json::from_slice(
        &std::fs::read(sandbox.data.join("marketplaces/installations.json")).unwrap(),
    )
    .unwrap();
    let snapshot = PathBuf::from(
        state["plugins"]["plugin-cursor@cursor"]["snapshot"]
            .as_str()
            .unwrap(),
    );
    let skill = snapshot.join("skills/skill-cursor/SKILL.md");
    std::fs::write(&skill, "local edit\n").unwrap();

    let guarded = sandbox.run(&["plugin", "update", "plugin-cursor", "--global"]);
    assert!(!guarded.status.success());
    assert!(String::from_utf8_lossy(&guarded.stderr).contains("local package edits"));
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), "local edit\n");

    sandbox.assert_success(&["plugin", "update", "plugin-cursor", "--force", "--global"]);
    let state: Value = serde_json::from_slice(
        &std::fs::read(sandbox.data.join("marketplaces/installations.json")).unwrap(),
    )
    .unwrap();
    let replacement = PathBuf::from(
        state["plugins"]["plugin-cursor@cursor"]["snapshot"]
            .as_str()
            .unwrap(),
    );
    assert!(
        std::fs::read_to_string(replacement.join("skills/skill-cursor/SKILL.md"))
            .unwrap()
            .contains("name: skill-cursor")
    );
    assert_eq!(std::fs::read_to_string(skill).unwrap(), "local edit\n");
    // A normal update must keep the repaired snapshot, and another identical local
    // edit must not cause a later forced repair to reuse an already-modified directory.
    for force in [false, true, false] {
        if force {
            std::fs::write(
                replacement.join("skills/skill-cursor/SKILL.md"),
                "local edit\n",
            )
            .unwrap();
        }
        let mut args = vec!["plugin", "update", "plugin-cursor", "--global"];
        if force {
            args.push("--force");
        }
        sandbox.assert_success(&args);
        let state: Value = serde_json::from_slice(
            &std::fs::read(sandbox.data.join("marketplaces/installations.json")).unwrap(),
        )
        .unwrap();
        let active = PathBuf::from(
            state["plugins"]["plugin-cursor@cursor"]["snapshot"]
                .as_str()
                .unwrap(),
        );
        assert!(
            std::fs::read_to_string(active.join("skills/skill-cursor/SKILL.md"))
                .unwrap()
                .contains("name: skill-cursor")
        );
    }
}

#[test]
fn project_scope_keeps_plugin_state_and_outputs_inside_project() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "codex");
    sandbox.assert_success(&["init", "--no-migrate"]);
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "codex",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-codex", "--no-enable"]);
    for target in ["codex", "claude", "cursor"] {
        sandbox.assert_success(&["plugin", "enable", "plugin-codex", "--agent", target]);
    }

    assert!(sandbox
        .workspace
        .join(".yasm/marketplaces/registry.json")
        .is_file());
    assert!(sandbox
        .workspace
        .join(".yasm/marketplaces/installations.json")
        .is_file());
    assert!(sandbox
        .workspace
        .join(".agents/skills/skill-codex")
        .is_symlink());
    assert!(
        !std::fs::read_link(sandbox.workspace.join(".agents/skills/skill-codex"))
            .unwrap()
            .is_absolute()
    );
    assert!(sandbox
        .workspace
        .join(".claude/skills/skill-codex")
        .is_symlink());
    assert!(sandbox
        .workspace
        .join(".cursor/skills/skill-codex")
        .is_symlink());
    assert!(sandbox.workspace.join(".codex/config.toml").is_file());
    assert!(sandbox.workspace.join(".mcp.json").is_file());
    assert!(sandbox.workspace.join(".cursor/mcp.json").is_file());
    assert!(!sandbox.data.join("marketplaces/registry.json").exists());
}

#[test]
fn list_groups_installed_plugins_and_summarizes_package_skills() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "codex");
    let plugin = source.join("plugins/plugin-codex");
    std::fs::create_dir_all(plugin.join("skills/second-skill")).unwrap();
    std::fs::write(
        plugin.join("skills/second-skill/SKILL.md"),
        "---\nname: second-skill\ndescription: second fixture\n---\n",
    )
    .unwrap();
    sandbox.assert_success(&["init", "--no-migrate"]);
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "codex",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-codex", "--agent", "codex"]);
    sandbox.assert_success(&["plugin", "enable", "plugin-codex", "--agent", "claude"]);

    let output = sandbox.assert_success(&["list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Project Plugins (1)"), "{stdout}");
    assert!(stdout.contains("plugin-codex@codex"), "{stdout}");
    assert!(stdout.contains("codex, claude"), "{stdout}");
    assert!(stdout.contains("2/2"), "{stdout}");
    assert!(stdout.contains("Summary"), "{stdout}");
    assert!(stdout.contains("codex plugin fixture"), "{stdout}");
    assert_eq!(stdout.matches("plugin-codex@codex").count(), 1, "{stdout}");

    let output = sandbox.assert_success(&["list", "--json"]);
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let plugins = json["project"]["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["id"], "plugin-codex@codex");
    assert_eq!(plugins[0]["description"], "codex plugin fixture");
    assert_eq!(plugins[0]["skill_count"], 2);
    assert_eq!(plugins[0]["mcp_server_count"], 2);
    assert_eq!(
        plugins[0]["enabled"],
        serde_json::json!(["codex", "claude"])
    );
    assert_eq!(plugins[0]["skills"].as_array().unwrap().len(), 2);
    assert_eq!(
        plugins[0]["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|skill| skill["name"] == "skill-codex")
            .unwrap()["enabled"],
        serde_json::json!(["codex", "claude"])
    );
    assert_eq!(
        plugins[0]["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|skill| skill["name"] == "second-skill")
            .unwrap()["enabled"],
        serde_json::json!(["codex", "claude"])
    );
}

#[test]
fn plugin_info_shows_complete_package_inventory() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "codex");
    let plugin = source.join("plugins/plugin-codex");
    std::fs::create_dir_all(plugin.join("skills/second-skill")).unwrap();
    std::fs::write(
        plugin.join("skills/second-skill/SKILL.md"),
        "---\nname: second-skill\ndescription: second fixture\n---\n",
    )
    .unwrap();
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "codex",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "plugin-codex",
        "--agent",
        "codex",
        "--global",
    ]);

    let output = sandbox.assert_success(&["plugin", "info", "plugin-codex", "--global"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Enabled targets:"), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("skill-codex") && line.contains("codex")),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("second-skill") && line.contains("codex")),
        "{stdout}"
    );
}

#[test]
fn component_aliases_resolve_collisions_and_survive_update_cleanup() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "codex");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "codex",
        "--global",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-codex", "--no-enable", "--global"]);
    sandbox.assert_success(&[
        "plugin",
        "enable",
        "plugin-codex",
        "--agent",
        "cursor",
        "--skill-alias",
        "skill-codex=renamed-skill",
        "--mcp-alias",
        "stdio-codex=renamed-mcp",
        "--global",
    ]);
    assert!(sandbox
        .agents
        .join(".cursor/skills/renamed-skill")
        .is_symlink());
    assert!(!sandbox.agents.join(".cursor/skills/skill-codex").exists());
    let cursor_path = sandbox.home.join(".cursor/mcp.json");
    let cursor: Value = serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
    assert!(cursor["mcpServers"]["renamed-mcp"].is_object());
    assert!(cursor["mcpServers"]["http-codex"].is_object());

    std::fs::write(
        source.join("plugins/plugin-codex/skills/skill-codex/SKILL.md"),
        "---\nname: skill-codex\ndescription: updated\n---\n",
    )
    .unwrap();
    sandbox.assert_success(&["plugin", "update", "plugin-codex", "--global"]);
    assert!(sandbox
        .agents
        .join(".cursor/skills/renamed-skill")
        .is_symlink());
    let cursor: Value = serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
    assert!(cursor["mcpServers"]["renamed-mcp"].is_object());

    sandbox.assert_success(&[
        "plugin",
        "disable",
        "plugin-codex",
        "--agent",
        "cursor",
        "--global",
    ]);
    assert!(!sandbox.agents.join(".cursor/skills/renamed-skill").exists());
    let cursor: Value = serde_json::from_slice(&std::fs::read(cursor_path).unwrap()).unwrap();
    assert!(cursor["mcpServers"]["renamed-mcp"].is_null());
}

#[test]
fn plugin_update_reconciles_added_and_removed_components() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "cursor");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "cursor",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "plugin-cursor",
        "--agent",
        "codex",
        "--global",
    ]);
    let plugin = source.join("plugins/plugin-cursor");
    std::fs::remove_dir_all(plugin.join("skills/skill-cursor")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/new-skill")).unwrap();
    std::fs::write(
        plugin.join("skills/new-skill/SKILL.md"),
        "---\nname: new-skill\ndescription: new\n---\n",
    )
    .unwrap();
    std::fs::write(
        plugin.join("mcp.json"),
        r#"{"mcpServers":{"new-server":{"command":"printf","args":["new"]}}}"#,
    )
    .unwrap();

    sandbox.assert_success(&["plugin", "update", "plugin-cursor", "--global"]);
    assert!(!sandbox.agents.join(".agents/skills/skill-cursor").exists());
    assert!(sandbox.agents.join(".agents/skills/new-skill").is_symlink());
    let codex = std::fs::read_to_string(sandbox.home.join(".codex/config.toml")).unwrap();
    assert!(!codex.contains("stdio-cursor"));
    assert!(!codex.contains("http-cursor"));
    assert!(codex.contains("[mcp_servers.new-server]"));
}

#[test]
fn unsupported_components_prevent_any_export() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    let plugin = source.join("plugins/plugin-claude");
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::write(plugin.join("agents/reviewer.md"), "agent prompt\n").unwrap();
    std::fs::write(
        plugin.join(".mcp.json"),
        r#"{"mcpServers":{"context7":{"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"${TOKEN:-}"}}}}"#,
    )
    .unwrap();
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-claude", "--no-enable", "--global"]);
    let guarded = sandbox.run(&[
        "plugin",
        "enable",
        "plugin-claude",
        "--agent",
        "cursor",
        "--global",
    ]);
    assert!(!guarded.status.success());
    assert!(String::from_utf8_lossy(&guarded.stderr).contains("all-or-nothing"));

    assert!(!sandbox.agents.join(".cursor/skills/skill-claude").exists());
    assert!(!sandbox.home.join(".cursor/mcp.json").exists());
}

#[test]
fn plugin_storage_keys_do_not_collapse_qualified_ids() {
    let sandbox = Sandbox::new();
    let first = create_single_skill_marketplace(
        sandbox.root.path(),
        "collision-first",
        "first",
        "foo--bar",
        "first-skill",
    );
    let second = create_single_skill_marketplace(
        sandbox.root.path(),
        "collision-second",
        "second",
        "foo",
        "second-skill",
    );
    sandbox.assert_success(&[
        "marketplace",
        "add",
        first.to_str().unwrap(),
        "--alias",
        "baz",
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&[
        "marketplace",
        "add",
        second.to_str().unwrap(),
        "--alias",
        "bar--baz",
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&["plugin", "add", "foo--bar@baz", "--no-enable", "--global"]);
    sandbox.assert_success(&["plugin", "add", "foo@bar--baz", "--no-enable", "--global"]);

    let state: Value = serde_json::from_slice(
        &std::fs::read(sandbox.data.join("marketplaces/installations.json")).unwrap(),
    )
    .unwrap();
    let first_snapshot = PathBuf::from(
        state["plugins"]["foo--bar@baz"]["snapshot"]
            .as_str()
            .unwrap(),
    );
    let second_snapshot = PathBuf::from(
        state["plugins"]["foo@bar--baz"]["snapshot"]
            .as_str()
            .unwrap(),
    );
    assert_ne!(first_snapshot.parent(), second_snapshot.parent());

    sandbox.assert_success(&["plugin", "remove", "foo--bar@baz", "--global"]);
    assert!(second_snapshot.is_dir());
    sandbox.assert_success(&["plugin", "info", "foo@bar--baz", "--global"]);
}

#[test]
fn update_preserves_catalog_skill_selection_after_marketplace_removal() {
    let sandbox = Sandbox::new();
    let source = sandbox.root.path().join("selected-root-marketplace");
    std::fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    for skill in ["one", "two"] {
        std::fs::create_dir_all(source.join("skills").join(skill)).unwrap();
        std::fs::write(
            source.join("skills").join(skill).join("SKILL.md"),
            format!("---\nname: {skill}\ndescription: {skill}\n---\n"),
        )
        .unwrap();
    }
    std::fs::write(
        source.join(".claude-plugin/marketplace.json"),
        r#"{"name":"selected","plugins":[{"name":"demo","source":".","skills":["./skills/one"]}]}"#,
    )
    .unwrap();
    std::fs::write(
        source.join(".claude-plugin/plugin.json"),
        r#"{"name":"demo"}"#,
    )
    .unwrap();
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "demo@selected",
        "--agent",
        "codex",
        "--global",
    ]);
    assert!(sandbox.agents.join(".agents/skills/one").is_symlink());
    assert!(!sandbox.agents.join(".agents/skills/two").exists());

    sandbox.assert_success(&["marketplace", "remove", "selected", "--global"]);
    std::fs::write(source.join("README.md"), "updated\n").unwrap();
    sandbox.assert_success(&["plugin", "update", "demo@selected", "--global"]);

    assert!(sandbox.agents.join(".agents/skills/one").is_symlink());
    assert!(!sandbox.agents.join(".agents/skills/two").exists());
    let info = sandbox.assert_success(&["plugin", "info", "demo@selected", "--json", "--global"]);
    let info: Value = serde_json::from_slice(&info.stdout).unwrap();
    assert_eq!(info["definition"]["skills"].as_array().unwrap().len(), 1);
    assert_eq!(info["definition"]["skills"][0]["name"], "one");
}

#[test]
fn unsafe_skill_prevents_entire_plugin_export() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    let unsafe_skill = source.join("plugins/plugin-claude/skills/unsafe");
    std::fs::create_dir_all(&unsafe_skill).unwrap();
    std::fs::write(
        unsafe_skill.join("SKILL.md"),
        "---\nname: unsafe\ncontext: \"fork\"\n---\nPrompt\n",
    )
    .unwrap();
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-claude", "--no-enable", "--global"]);

    let guarded = sandbox.run(&[
        "plugin",
        "enable",
        "plugin-claude",
        "--agent",
        "codex",
        "--global",
    ]);
    assert!(!guarded.status.success());
    assert!(String::from_utf8_lossy(&guarded.stderr).contains("all-or-nothing"));

    assert!(!sandbox.agents.join(".agents/skills/skill-claude").exists());
    assert!(!sandbox.agents.join(".agents/skills/unsafe").exists());
    assert!(!sandbox.home.join(".codex/config.toml").exists());
}

#[test]
fn conflicting_mcp_definitions_are_never_arbitrarily_deployed() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    std::fs::write(
        source.join("plugins/plugin-claude/.claude-plugin/plugin.json"),
        r#"{"name":"plugin-claude","mcpServers":[{"same":{"command":"one"}},{"same":{"command":"two"}}]}"#,
    )
    .unwrap();
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-claude", "--no-enable", "--global"]);

    let guarded = sandbox.run(&[
        "plugin",
        "enable",
        "plugin-claude",
        "--agent",
        "codex",
        "--global",
    ]);
    assert!(!guarded.status.success());
    assert!(String::from_utf8_lossy(&guarded.stderr).contains("all-or-nothing"));

    assert!(!sandbox.agents.join(".agents/skills/skill-claude").exists());
    assert!(!sandbox.agents.join(".agents/skills/unsafe").exists());
    assert!(!sandbox.home.join(".codex/config.toml").exists());
}

#[test]
fn manually_edited_credentials_are_redacted_in_output_and_remain_removable() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    std::fs::write(
        source.join("plugins/plugin-claude/.mcp.json"),
        r#"{"mcpServers":{"stdio-claude":{"command":"printf","args":["ready"]},"http-claude":{"type":"http","url":"https://example.test/claude/mcp","headers":{"Authorization":"Bearer ${TOKEN}"}}}}"#,
    )
    .unwrap();
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "plugin-claude",
        "--agent",
        "claude",
        "--global",
    ]);

    let state_path = sandbox.data.join("marketplaces/installations.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    // State may be edited outside Yasm. User-facing views must still redact credentials.
    let plugin = &mut state["plugins"]["plugin-claude@claude"]["definition"];
    let http = plugin["mcpServers"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|server| server["name"] == "http-claude")
        .unwrap();
    http["headers"] = serde_json::json!({"Authorization": "Bearer top-secret"});
    http["url"] = Value::String("https://example.test/mcp?token=top-secret".to_string());
    http["extensions"] = serde_json::json!({"vendor": {"credential": "top-secret"}});
    plugin["inputs"] = serde_json::json!([{
        "name": "token",
        "type": "password",
        "required": false,
        "sensitive": true,
        "default": "top-secret",
        "constraints": {"vendor": "top-secret"}
    }]);
    state["plugins"]["plugin-claude@claude"]["catalog_entry"]["raw"] =
        serde_json::json!({"credential": "top-secret"});
    std::fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

    let claude_path = sandbox.home.join(".claude.json");
    let mut claude: Value = serde_json::from_slice(&std::fs::read(&claude_path).unwrap()).unwrap();
    claude["mcpServers"]["http-claude"]["headers"]["Authorization"] =
        Value::String("Bearer top-secret".to_string());
    claude["mcpServers"]["http-claude"]["url"] =
        Value::String("https://example.test/mcp?token=top-secret".to_string());
    std::fs::write(&claude_path, serde_json::to_vec_pretty(&claude).unwrap()).unwrap();

    for args in [
        vec!["plugin", "info", "plugin-claude", "--json", "--global"],
        vec!["plugin", "list", "--json", "--global"],
    ] {
        let output = sandbox.assert_success(&args);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.contains("top-secret"), "{stdout}");
        assert!(stdout.contains("[redacted]"), "{stdout}");
    }

    let rejected = sandbox.run(&[
        "plugin",
        "enable",
        "plugin-claude",
        "--agent",
        "cursor",
        "--global",
    ]);
    assert!(!rejected.status.success());
    assert!(!String::from_utf8_lossy(&rejected.stderr).contains("top-secret"));
    assert!(!sandbox.home.join(".cursor/mcp.json").exists());
    let persisted = std::fs::read_to_string(&state_path).unwrap();
    assert!(persisted.contains("top-secret"), "{persisted}");

    sandbox.assert_success(&[
        "plugin",
        "disable",
        "plugin-claude",
        "--agent",
        "claude",
        "--global",
    ]);
    let claude: Value = serde_json::from_slice(&std::fs::read(&claude_path).unwrap()).unwrap();
    assert!(claude["mcpServers"]["http-claude"].is_null());

    sandbox.assert_success(&["plugin", "remove", "plugin-claude", "--all", "--global"]);
    let persisted = std::fs::read_to_string(&state_path).unwrap();
    assert!(!persisted.contains("top-secret"), "{persisted}");
}

#[test]
fn edited_mcp_argument_credentials_are_hidden_and_rejected_on_enable() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-claude", "--no-enable", "--global"]);

    let state_path = sandbox.data.join("marketplaces/installations.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    let servers = state["plugins"]["plugin-claude@claude"]["definition"]["mcpServers"]
        .as_array_mut()
        .unwrap();
    let server = servers
        .iter_mut()
        .find(|server| server["name"] == "http-claude")
        .unwrap();
    server["args"] = serde_json::json!(["--endpoint=https://example.test/mcp?sig=top-secret"]);
    std::fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

    let info = sandbox.assert_success(&["plugin", "info", "plugin-claude", "--json", "--global"]);
    let stdout = String::from_utf8(info.stdout).unwrap();
    assert!(!stdout.contains("top-secret"), "{stdout}");
    assert!(stdout.contains("[redacted]"), "{stdout}");

    let rejected = sandbox.run(&[
        "plugin",
        "enable",
        "plugin-claude",
        "--agent",
        "claude",
        "--global",
    ]);
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("argument 0"), "{stderr}");
    assert!(!stderr.contains("top-secret"), "{stderr}");
}

#[test]
fn deterministic_stdio_fixture_serves_a_real_mcp_tool_call() {
    use std::io::Write;

    let server = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../yasm-marketplace/tests/fixtures/runtime-marketplace/plugins/ping/server.py");
    let mut child = Command::new("python3")
        .arg(server)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"ping\",\"arguments\":{}}}\n"
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(requests.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses = String::from_utf8(output.stdout).unwrap();
    assert!(responses.contains("\"name\":\"ping\""));
    assert!(responses.contains("pong-yasm"));
}

fn create_marketplace(root: &Path, format: &str) -> PathBuf {
    let source = root.join(format!("marketplace-{format}"));
    let catalog_path = match format {
        "codex" => ".agents/plugins/marketplace.json",
        "claude" => ".claude-plugin/marketplace.json",
        "cursor" => ".cursor-plugin/marketplace.json",
        _ => unreachable!(),
    };
    let plugin = source.join(format!("plugins/plugin-{format}"));
    std::fs::create_dir_all(source.join(catalog_path).parent().unwrap()).unwrap();
    std::fs::create_dir_all(plugin.join(format!("skills/skill-{format}"))).unwrap();
    std::fs::write(
        source.join(catalog_path),
        format!(
            r#"{{"name":"{format}","plugins":[{{"name":"plugin-{format}","description":"{format} plugin fixture","source":"./plugins/plugin-{format}"}}]}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        plugin.join(format!("skills/skill-{format}/SKILL.md")),
        format!("---\nname: skill-{format}\ndescription: {format} fixture\n---\n"),
    )
    .unwrap();
    let manifest = match format {
        "codex" => plugin.join("plugin.json"),
        "claude" => plugin.join(".claude-plugin/plugin.json"),
        "cursor" => plugin.join(".cursor-plugin/plugin.json"),
        _ => unreachable!(),
    };
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(
        manifest,
        format!(r#"{{"name":"plugin-{format}","version":"1.0.0"}}"#),
    )
    .unwrap();
    let mcp_path = if format == "claude" {
        plugin.join(".mcp.json")
    } else {
        plugin.join("mcp.json")
    };
    std::fs::write(
        mcp_path,
        format!(
            r#"{{"mcpServers":{{"stdio-{format}":{{"command":"printf","args":["ready"]}},"http-{format}":{{"type":"http","url":"https://example.test/{format}/mcp"}}}}}}"#
        ),
    )
    .unwrap();
    source
}

fn create_single_skill_marketplace(
    root: &Path,
    directory: &str,
    marketplace: &str,
    plugin_name: &str,
    skill_name: &str,
) -> PathBuf {
    let source = root.join(directory);
    let plugin = source.join("plugin");
    std::fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("skills").join(skill_name)).unwrap();
    std::fs::write(
        source.join(".claude-plugin/marketplace.json"),
        format!(
            r#"{{"name":"{marketplace}","plugins":[{{"name":"{plugin_name}","source":"./plugin"}}]}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        plugin.join("skills").join(skill_name).join("SKILL.md"),
        format!("---\nname: {skill_name}\n---\n"),
    )
    .unwrap();
    source
}

#[test]
fn partial_flag_is_rejected_for_every_mutation() {
    let sandbox = Sandbox::new();
    for action in ["add", "enable", "update"] {
        let output = sandbox.run(&["plugin", action, "demo", "--allow-partial"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("unexpected argument '--allow-partial'"));
    }
}

#[test]
fn root_skills_export_copies_without_native_plugin_registration() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    let package = source.join("plugins/plugin-claude");
    std::fs::remove_dir_all(package.join("skills")).unwrap();
    std::fs::write(
        package.join("SKILL.md"),
        "---\nname: root-skill\n---\nUse scripts/tool.sh and references/guide.md.\n",
    )
    .unwrap();
    for (path, contents) in [
        ("scripts/tool.sh", "echo hello"),
        ("references/guide.md", "Guide"),
        ("assets/example.txt", "Example"),
        (
            "agents/openai.yaml",
            "interface:\n  display_name: Root skill\n",
        ),
    ] {
        std::fs::create_dir_all(package.join(path).parent().unwrap()).unwrap();
        std::fs::write(package.join(path), contents).unwrap();
    }
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&["plugin", "add", "plugin-claude", "--no-enable", "--global"]);
    for (target, dir) in [
        ("claude", ".claude"),
        ("codex", ".agents"),
        ("cursor", ".cursor"),
    ] {
        sandbox.assert_success(&[
            "plugin",
            "enable",
            "plugin-claude",
            "--agent",
            target,
            "--global",
        ]);
        let link = sandbox.agents.join(dir).join("skills/root-skill");
        assert!(link.is_symlink());
        assert!(std::fs::read_link(&link)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("/outputs/"));
        for path in [
            "SKILL.md",
            "scripts/tool.sh",
            "references/guide.md",
            "assets/example.txt",
            "agents/openai.yaml",
        ] {
            assert!(link.join(path).is_file(), "missing {path}");
        }
        for path in [".claude-plugin", ".mcp.json", "hooks", "plugin.json"] {
            assert!(!link.join(path).exists(), "exposed {path}");
        }
    }
    let state: Value = serde_json::from_slice(
        &std::fs::read(sandbox.data.join("marketplaces/installations.json")).unwrap(),
    )
    .unwrap();
    let snapshot = PathBuf::from(
        state["plugins"]["plugin-claude@claude"]["snapshot"]
            .as_str()
            .unwrap(),
    );
    assert!(snapshot.join(".claude-plugin/plugin.json").is_file());
    sandbox.assert_success(&["plugin", "update", "plugin-claude", "--global"]);
    for dir in [".claude", ".agents", ".cursor"] {
        let link = sandbox.agents.join(dir).join("skills/root-skill");
        assert!(link.join("SKILL.md").is_file());
        assert!(!link.join(".claude-plugin").exists());
    }
    sandbox.assert_success(&["plugin", "remove", "plugin-claude", "--all", "--global"]);
    assert!(package.join(".claude-plugin/plugin.json").is_file());
}

#[cfg(unix)]
#[test]
fn failed_link_rolls_back_mcp_entries_and_can_be_retried() {
    for acquired in [false, true] {
        for (target, skill_dir, config) in [
            ("claude", ".claude/skills", ".claude.json"),
            ("codex", ".agents/skills", ".codex/config.toml"),
            ("cursor", ".cursor/skills", ".cursor/mcp.json"),
        ] {
            let sandbox = Sandbox::new();
            let source = create_marketplace(sandbox.root.path(), "claude");
            sandbox.assert_success(&[
                "marketplace",
                "add",
                source.to_str().unwrap(),
                "--format",
                "claude",
                "--global",
            ]);
            if acquired {
                sandbox.assert_success(&[
                    "plugin",
                    "add",
                    "plugin-claude",
                    "--no-enable",
                    "--global",
                ]);
            }
            let config = sandbox.home.join(config);
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            let user_config = if target == "codex" {
                "model = \"user-model\"\n[mcp_servers.user]\ncommand = \"user-server\"\n"
            } else {
                r#"{"theme":"dark","mcpServers":{"user":{"command":"user-server"}}}"#
            };
            std::fs::write(&config, user_config).unwrap();
            let skill_parent = sandbox.agents.join(skill_dir);
            std::fs::create_dir_all(skill_parent.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink("missing-skills", &skill_parent).unwrap();
            let action = if acquired { "enable" } else { "add" };
            let args = [
                "plugin",
                action,
                "plugin-claude",
                "--agent",
                target,
                "--global",
            ];
            let failed = sandbox.run(&args);
            assert!(!failed.status.success());
            let after = std::fs::read_to_string(&config).unwrap();
            assert!(after.contains("user-server"));
            assert!(!after.contains("stdio-claude"), "{after}");
            assert!(!after.contains("http-claude"), "{after}");
            let state_path = sandbox.data.join("marketplaces/installations.json");
            if acquired {
                let state: Value =
                    serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
                assert!(state["plugins"]["plugin-claude@claude"]["enabled"]
                    .as_array()
                    .unwrap()
                    .is_empty());
            } else {
                assert!(!state_path.exists());
            }
            assert_eq!(
                std::fs::read_dir(sandbox.data.join("marketplaces/operations"))
                    .unwrap()
                    .count(),
                0
            );
            std::fs::remove_file(skill_parent).unwrap();
            sandbox.assert_success(&args);
            sandbox.assert_success(&["plugin", "remove", "plugin-claude", "--all", "--global"]);
        }
    }
}

#[test]
fn unsupported_update_preserves_previous_outputs_and_receipt() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "plugin-claude",
        "--agent",
        "claude",
        "--global",
    ]);
    let state_path = sandbox.data.join("marketplaces/installations.json");
    let before = std::fs::read(&state_path).unwrap();
    let config = std::fs::read(sandbox.home.join(".claude.json")).unwrap();
    let link = sandbox.agents.join(".claude/skills/skill-claude");
    let target = std::fs::read_link(&link).unwrap();
    std::fs::create_dir_all(source.join("plugins/plugin-claude/hooks")).unwrap();
    std::fs::write(source.join("plugins/plugin-claude/hooks/hooks.json"), "{}").unwrap();
    let rejected = sandbox.run(&["plugin", "update", "plugin-claude", "--global"]);
    assert!(!rejected.status.success());
    assert_eq!(std::fs::read(state_path).unwrap(), before);
    assert_eq!(
        std::fs::read(sandbox.home.join(".claude.json")).unwrap(),
        config
    );
    assert_eq!(std::fs::read_link(link).unwrap(), target);
}

#[cfg(unix)]
#[test]
fn skill_resources_materialize_internal_links_and_reject_harness_configuration() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    let package = source.join("plugins/plugin-claude");
    let skill = package.join("skills/skill-claude");
    std::fs::create_dir_all(package.join("shared")).unwrap();
    std::fs::write(package.join("shared/tool.sh"), "echo ready").unwrap();
    std::os::unix::fs::symlink("../../shared", skill.join("scripts")).unwrap();
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "plugin-claude",
        "--agent",
        "claude",
        "--global",
    ]);
    let output = sandbox.agents.join(".claude/skills/skill-claude");
    assert!(!output.join("scripts").is_symlink());
    assert_eq!(
        std::fs::read_to_string(output.join("scripts/tool.sh")).unwrap(),
        "echo ready"
    );
    std::fs::write(package.join("shared/hooks.json"), "{}").unwrap();
    let rejected = sandbox.run(&["plugin", "update", "plugin-claude", "--global"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unsupported harness configuration"));
    assert!(!output.join("scripts/hooks.json").exists());
}

#[test]
fn unchanged_update_still_detects_locally_modified_mcp_entry() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "plugin-claude",
        "--agent",
        "claude",
        "--global",
    ]);
    let config_path = sandbox.home.join(".claude.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["mcpServers"]["stdio-claude"]["command"] = Value::String("edited".into());
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let before = std::fs::read(sandbox.data.join("marketplaces/installations.json")).unwrap();
    let rejected = sandbox.run(&["plugin", "update", "plugin-claude", "--global"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("modified MCP entry"));
    assert_eq!(
        before,
        std::fs::read(sandbox.data.join("marketplaces/installations.json")).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn plugin_update_preserves_exported_script_executable_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    let package = source.join("plugins/plugin-claude");
    let script = package.join("skills/skill-claude/scripts/run.sh");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    std::fs::write(&script, "#!/bin/sh\necho ready\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "plugin-claude",
        "--agent",
        "claude",
        "--global",
    ]);
    let output = sandbox
        .agents
        .join(".claude/skills/skill-claude/scripts/run.sh");
    assert_eq!(
        std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o644
    );
    std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        package.join(".claude-plugin/plugin.json"),
        r#"{"name":"plugin-claude","version":"1.0.1"}"#,
    )
    .unwrap();
    sandbox.assert_success(&["plugin", "update", "plugin-claude", "--global"]);
    assert_eq!(
        std::fs::metadata(output).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn incomplete_plugin_state_is_rejected_without_changing_outputs() {
    let sandbox = Sandbox::new();
    let source = create_marketplace(sandbox.root.path(), "claude");
    sandbox.assert_success(&[
        "marketplace",
        "add",
        source.to_str().unwrap(),
        "--format",
        "claude",
        "--global",
    ]);
    sandbox.assert_success(&[
        "plugin",
        "add",
        "plugin-claude",
        "--agent",
        "claude",
        "--global",
    ]);
    let state_path = sandbox.data.join("marketplaces/installations.json");
    let current: Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    let config_path = sandbox.home.join(".claude.json");
    let config = std::fs::read(&config_path).unwrap();
    for field in ["catalog_entry", "skill_exports", "mcp_sources"] {
        let mut state = current.clone();
        let plugin = &mut state["plugins"]["plugin-claude@claude"];
        if field == "catalog_entry" {
            plugin.as_object_mut().unwrap().remove(field);
        } else {
            plugin["outputs"][0].as_object_mut().unwrap().remove(field);
        }
        let bytes = serde_json::to_vec_pretty(&state).unwrap();
        std::fs::write(&state_path, &bytes).unwrap();
        let output = sandbox.run(&["plugin", "remove", "plugin-claude", "--all", "--global"]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("missing field `{field}`"))
        );
        assert_eq!(std::fs::read(&state_path).unwrap(), bytes);
        assert_eq!(std::fs::read(&config_path).unwrap(), config);
    }
}

# Yasm

Manage AI agent skills globally or per project. Yasm helps you install skills,
track where they came from, keep them updated, and control which agents can use
them.

Yasm is built around a reusable Rust library. Its CLI uses that library, and
other projects can build on the same skill-management functionality.

[Installation](#installation) · [Quick start](#quick-start) ·
[Everyday usage](#everyday-usage) · [Use as a library](#use-as-a-library)

## Why Yasm?

Managing skills by hand means copying directories between tools, remembering
their sources, and repeating the work for each project. Yasm gives that work a
consistent workflow:

- **Manage global and project skills.** Keep personal skills available across
  projects and project-specific skills alongside your code.
- **Share skills across agents.** Store a skill once per scope and enable it for
  the agent directories that need it.
- **Track sources and updates.** Install from GitHub or local directories, adopt
  existing skills, and update skills from their recorded sources.
- **Share a working setup with teammates.** Commit project skills and their links
  so a clone includes the skills without requiring Yasm to use them.
- **Build on the library.** Reuse skill discovery, storage, metadata, and lifecycle
  operations in other Rust tools.

## Installation

Install the latest stable release on Linux or macOS:

```sh
curl -fsSL https://yasm.itzlambda.com/install | sh
```

Requires `git`. On Windows, use WSL with your project on the Linux filesystem.

To upgrade:

```sh
yasm self-upgrade
```

## Quick start

From the project where you want to use skills:

```bash
# Create a project store without adopting existing skills.
yasm init --no-migrate

# Install a skill from GitHub and enable it for all built-in agent targets.
yasm add anthropics/skills --skill frontend-design --action apply

# See installed skills and inspect the one you added.
yasm list
yasm info frontend-design
```

The skill is stored under `.yasm/skills/` and linked into `.agents/skills/` and
`.claude/skills/`. If you already have skills to bring into Yasm, see
[adopting existing skills](#adopt-existing-skills).

To install the same skill globally, run this from any directory; no `init` is
needed:

```bash
yasm add anthropics/skills --skill frontend-design --global --action apply
```

## Everyday usage

### Adopt existing skills

Preview the skills Yasm can adopt from `.agents/skills/` and `.claude/skills/`,
then initialize the project and move them into its store:

```bash
yasm init --action review
yasm init --action apply
```

For an existing Yasm project, use `yasm migrate --action apply`. Add `--global`
to adopt global skills. Migration preserves installed files and lets you choose
an upstream source for future updates.

To adopt a specific skill without prompting:

```bash
yasm migrate --skill team-review --source local --action apply
```

See `yasm migrate --help` for source selection and filtering options.

### Add more skills from a source

Running `yasm add` again shows already-installed skills in a separate checked
list and offers only additional skills in the picker. If every discovered skill
is installed in the selected scope, the command exits without prompting. Adding
skills preserves existing sibling skills and their agent settings.

Select an installed skill explicitly to refresh its contents or repair missing
files and managed links:

```bash
yasm add owner/repo --skill retro --action apply
```

When multiple skills share a name, pass their repository-relative `SKILL.md`
path to `--skill` to choose one explicitly.

A skill ID already acquired from a different source requires `--replace`:

```bash
yasm add another/repo --skill retro --replace --action apply
```

GitHub shorthand and repository URLs share the same source identity. Skill paths
in installation receipts are relative to the repository root, including when
installation starts from a subdirectory URL. Default-branch sources continue to
follow the remote default branch; explicitly selected branches and tags retain
their requested ref.

Add and update reuse a repository cache shared across project and global scopes.
Commit snapshots keep source files stable during review and installation.
Installed files are independent of this cache, so deleting it does not remove
installed skills. Fetching a newer commit does not update receipts or contents
for unselected skills.

### Private repositories over SSH

Use an SCP-style Git address to install skills from a private repository:

```bash
yasm add git@github.com:team/private-skills.git --skill code-review --global --action apply
```

Set up repository access and host trust with Git and SSH first. Yasm uses your
existing SSH configuration and keys, but does not prompt for passwords or
passphrases. GitHub `owner/repo` shorthand uses HTTPS; use an SCP-style address
for SSH.

Committing `.yasm/` shares installed skill contents with everyone who can access
the project, even when the upstream repository is private.

### Choose which agents use a skill

Enable a new skill for Claude only, or install it without enabling any agent:

```bash
yasm add ./skills --skill frontend-design --agent claude --action apply
yasm add ./skills --skill another-skill --no-enable --action apply
```

Here, `./skills` is a local directory containing your skills. Change agent
visibility later without fetching the skill again:

```bash
yasm enable frontend-design --agent universal
yasm disable frontend-design --agent claude
```

The built-in targets are `universal` (`.agents/skills/`) and `claude`
(`.claude/skills/`). Repeat `--agent` to select both explicitly.

### Update or remove a skill

```bash
# Update from the recorded source without prompting.
yasm update frontend-design --action apply

# Disable for every agent and delete the stored skill.
yasm remove frontend-design --all
```

In a terminal, use `yasm update frontend-design --action review` to review the
changes and choose whether to apply them. Skills with no upstream source are
locally owned and are not checked for updates.

### Inspect and repair your setup

```bash
yasm list --enabled
yasm list --global
yasm status
yasm doctor --repair
```

`status` reports store and link health; `doctor --repair` repairs missing or
dangling Yasm-owned links. For scripts, commands such as `list` and `status`
also support `--json`. Supply choices explicitly in scripts: non-TTY commands
never prompt and report any missing arguments.

### Manage plugin marketplaces (experimental)

**Marketplace and plugin support is experimental and may change incompatibly.**
Yasm supports Codex, Claude Code, and Cursor marketplace formats.

```bash
# Register a catalog and browse its plugins.
yasm marketplace add ./my-marketplace --alias local
yasm plugin list --available --marketplace local

# Install a plugin for Codex.
yasm plugin add example@local --agent codex
yasm plugin info example@local

# Update or remove it.
yasm plugin update example@local
yasm plugin remove example@local --all
```

Plugins can expose skills and supported MCP definitions. Plugins with unsupported
components cannot be enabled; use `--no-enable` to install them for inspection.
Use environment or input references for credentials rather than literal secrets
in a catalog, and treat local marketplace state and package snapshots as sensitive.

See [plugin runtime validation](docs/plugin-runtime-validation.md) for tested
client versions and outstanding compatibility checks.

## How it works

Yasm separates **storing a skill** from **enabling it for an agent**. `add` copies
a skill into the selected store and enables it by default. Enabling creates a
symlink in an agent's skill directory; disabling removes that link while keeping
the stored skill. Use `--no-enable` with `add` to store a skill without linking it.

Commands use the nearest `.yasm/` project store, searching from your current
directory upward. Outside a project, they use the global store. Pass `--global`
to manage global skills from inside a project. `init` always initializes the
current directory.

`list` shows both project and global skills when you are in a project. Commands
that change skills target only the selected store; use `--global` to change the
global copy, even if that skill is absent from the project store.

For a shared project, commit `.yasm/` (the lockfile and skill contents) and the
relative symlinks under `.agents/skills/` and `.claude/skills/`. A teammate who
clones the repository gets working skills without running Yasm. Fetch caches
live outside the project store and should not be committed.

## Use as a library

The workspace separates reusable functionality from the command-line interface:

| Crate | Role |
| --- | --- |
| [`yasm-core`](crates/yasm-core/src/lib.rs) | Skill discovery, metadata, stores, lockfiles, agent links, and lifecycle operations. |
| [`yasm-providers`](crates/yasm-providers/src/lib.rs) | Source parsing and fetching for GitHub, local directories, and bundled skills. |
| [`yasm-marketplace`](crates/yasm-marketplace/src/lib.rs) | Experimental marketplace import, package snapshots, and target-specific plugin deployment. |
| [`yasm-cli`](crates/yasm-cli/src/main.rs) | Commands, interactive choices, and terminal output built on the libraries. |

The library APIs are evolving; pin a Git revision for integrations that need a
fixed dependency. Generate API documentation from a checkout with
`cargo doc --workspace --no-deps --open`.

## Help

- `yasm --help` and `yasm <command> --help`: available commands and flags.
- [GitHub issues](https://github.com/itzlambda/yasm/issues): questions, bug reports,
  and feature requests.

## Contributing

To build from source, install Rust and Cargo, then:

```sh
git clone https://github.com/itzlambda/yasm.git
cd yasm
cargo install --path crates/yasm-cli --locked
```

Make sure `~/.cargo/bin` is on your `PATH`. During development, use
`cargo run -p yasm-cli -- --help` to run the CLI from your checkout.

Read [AGENTS.md](AGENTS.md) for project conventions. Before submitting changes,
install [Just](https://github.com/casey/just) and run `just lint` to check workspace
dependencies, formatting, Clippy warnings, and tests.

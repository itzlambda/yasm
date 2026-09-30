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

Install the latest stable release with [scripts/install.sh](scripts/install.sh).
A tagged build is published as a prerelease first. Mark it as a full release
after verifying it; until then the installer and `yasm self-upgrade` leave it
alone.

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/itzlambda/yasm/main/scripts/install.sh | sh
```

The script checks the release checksum and installs `yasm` to `~/.yasm/bin`.
When a terminal is available it asks before adding that directory to your `PATH`.
It updates the startup files of every shell it finds: `~/.profile`, `~/.bashrc`,
`~/.zshenv`, and `~/.config/fish/conf.d/yasm.fish`. If `~/.local/bin` is already
on your `PATH`, it also links `yasm` there so the current shell can run it
immediately. Pass `-y` to accept the `PATH` change without a prompt:

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/itzlambda/yasm/main/scripts/install.sh | sh -s -- -y
```

Yasm also requires `git` on `PATH` to fetch GitHub sources.

Upgrade that install from later GitHub releases with:

```bash
yasm self-upgrade
```

In a terminal, `yasm self-upgrade` asks before replacing the binary. Pass `--yes`
to upgrade without a prompt. It installs the newest stable release.

The script installs one of these binaries:

| Platform | Binary |
| --- | --- |
| Linux x86_64 | `yasm-x86_64-unknown-linux-musl` |
| Linux arm64 | `yasm-aarch64-unknown-linux-musl` |
| macOS Apple Silicon | `yasm-aarch64-apple-darwin` |
| macOS Intel | `yasm-x86_64-apple-darwin` |

Linux binaries are statically linked. macOS binaries are unsigned. The install
script removes the download quarantine attribute. If Gatekeeper still blocks the
first launch, open the binary from Finder. A `cargo install` build on Linux uses
the gnu target, so `yasm self-upgrade` does not match these musl binaries.

To build from source instead, install Rust and Cargo, then:

```bash
git clone https://github.com/itzlambda/yasm.git
cd yasm
cargo install --path crates/yasm-cli --locked
yasm --version
```

Cargo installs into `~/.cargo/bin`, which also needs to be on your `PATH`.

Linux and macOS are supported. On Windows, use WSL with your project on the Linux
filesystem. Native Windows requires Git `core.symlinks=true` and permission to
create directory symlinks.

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

For an existing Yasm project, use `yasm migrate --action apply`. To adopt global
skills, use `yasm migrate --global --action apply`. In an interactive terminal,
Yasm first shows recorded and recommended GitHub sources and lets you accept
them, then asks whether each remaining skill should be kept as local, assigned a
different GitHub source, or left unmanaged for now. Migration preserves the
installed files; accepting a source only configures where a later update checks.
The original installed revision remains unknown until an applied update verifies
the complete installed tree or installs fetched content.

The same choices are available without prompts:

```bash
# Adopt only candidates with recorded or recommended upstreams.
yasm migrate --with-upstream --action apply

# Keep one candidate as a locally owned skill.
yasm migrate --skill team-review --source local --action apply

# Validate and attach a different GitHub source while preserving installed files.
yasm migrate --skill humanizer --source blader/humanizer --action apply
```

`--skill` may be repeated to restrict the eligible set. A GitHub `--source`
requires exactly one selected skill; `--source local` can apply to several.
Omitting both `--with-upstream` and `--source` retains the existing
`--action apply` behavior of adopting every eligible candidate. Use the same
options with `init` when creating a project store.

Global migration follows symlinked agent directories (including symlinked
`.agents` or `.claude` parents), preserving those directory links. When agents
share a physical skill directory, each skill is migrated once and enabled for
both agents. Recovery checks that the directory still resolves to its recorded
location before restoring files. Project migration continues to skip symlinked
agent directories.

If an unmanaged agent directory has the same skill ID as a managed skill,
migration compares their complete file trees. An identical copy is adopted by
linking it to the existing store while preserving the stored skill and its lock
metadata. Different copies, a missing store directory, or a store directory
without a lock record appear as conflicts during review and must be repaired
before that skill can be applied. Use `--skill <name>` to migrate unrelated
skills without applying a reported conflict.

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

Yasm supports only its current stored-state formats: version 3 for `yasm.lock`
and version 1 for marketplace state, with current receipt and journal fields.
There is no automatic upgrade of older development state. Recreate unsupported
state with the current executable; keep skill files and any recovery backups
until that is complete.

### Manage plugin marketplaces (experimental)

**Marketplace and plugin support is experimental.** Commands, behavior, and
stored data formats may change incompatibly, and support may be removed in a
future release.

The default `marketplace` feature imports Codex, Claude Code, and Cursor
marketplace formats into one Yasm-owned lifecycle. Catalog format, package
format, and output target are independent, so a portable skill from one source
can be enabled for another supported agent.

```bash
# Register and inspect a catalog.
yasm marketplace add ./my-marketplace --alias local
yasm plugin list --available --marketplace local

# Snapshot a package and expose its supported components to Codex.
yasm plugin add example@local --agent codex
yasm plugin info example@local

# Show standalone skills plus one summary row per installed plugin.
yasm list

# Reconcile or remove the managed outputs later.
yasm plugin update example@local
yasm plugin remove example@local --all
```

Yasm snapshots the complete package, while only skills and supported MCP
definitions are exported. Enablement is all-or-nothing: plugins with unsupported
components or configuration cannot be enabled. Use `--no-enable` to acquire a
package for inspection with `yasm plugin info`; there is no partial-export flag.
Yasm does not provide per-component selection.

Marketplace import rejects literal credentials in recognised MCP environment
variables and headers, MCP URLs, credential URLs embedded in commands and
arguments, and nested extension data, including URL `sig` parameters. It also
rejects defaults on sensitive inputs and credentials
inside other input defaults or constraints. Symbolic environment and input
references remain available for target configuration. JSON inspection and debug
output redact non-symbolic MCP environment and header values, credential URLs
embedded in commands, arguments, source URLs, and nested input data, sensitive
input defaults, and retained extension data. Internal state and target rendering
keep the original values needed for reconciliation; treat marketplace state and
package snapshots as sensitive files.

Skill links point to isolated copies containing `SKILL.md`, `scripts/`,
`references/`, `assets/`, and optional `agents/openai.yaml`. Native plugin
manifests and configuration are never exposed through those links. Other
package files remain in the source snapshot, where MCP commands can use them.
Name collisions require explicit `--skill-alias` or `--mcp-alias` mappings.

Output changes and installation receipts are journaled together. Failed
operations roll back their output changes; interrupted operations recover on
the next marketplace or plugin mutation. Recovery and receipt-based cleanup
refuse to overwrite entries or links changed outside Yasm.

Marketplace state is independent from the ordinary `yasm.lock` schema and the
feature remains removable. `yasm list` reads that state to present installed
plugins once per project/global scope with enabled/installed component counts;
the terminal table adds a concise package summary and JSON retains the full
description. `yasm plugin info` shows the complete inventory and each
component's enabled targets. Neither command copies plugin records into
`yasm.lock`. Disable or remove managed outputs before building without the
feature:

```bash
yasm plugin remove example@local --all
cargo build -p yasm-cli --no-default-features
```

See [plugin runtime validation](docs/plugin-runtime-validation.md) for tested
client versions and the remaining live-client acceptance checks.

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

For example, another Rust project can discover skills in a local directory
without invoking the CLI. Add these dependencies to that project's `Cargo.toml`:

```toml
[dependencies]
camino = "1"
yasm-core = { git = "https://github.com/itzlambda/yasm.git" }
```

```rust
use camino::Utf8Path;
use yasm_core::{discover_skills, Result};

fn main() -> Result<()> {
    for skill in discover_skills(Utf8Path::new("./skills"))? {
        println!("{}: {}", skill.name.as_str(), skill.directory);
    }
    Ok(())
}
```

The crates are currently version `0.0.1`; treat the APIs as evolving and pin a Git
revision for integrations that need a fixed dependency. From a Yasm checkout,
generate API documentation with `cargo doc --workspace --no-deps --open`.

## Help

- `yasm --help` and `yasm <command> --help`: available commands and flags.
- [GitHub issues](https://github.com/itzlambda/yasm/issues): questions, bug reports,
  and feature requests.

## Contributing

Clone the repository using the installation steps above. Use
`cargo run -p yasm-cli -- --help` to run the CLI from your checkout and
`cargo test --workspace --all-features` to run the tests.

Read [AGENTS.md](AGENTS.md) for project conventions. Before submitting a pull
request, install [Just](https://github.com/casey/just) and run `just lint`, which
checks workspace dependencies, formatting, Clippy warnings, and tests.

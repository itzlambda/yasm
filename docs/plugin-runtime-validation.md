# Plugin runtime validation

Marketplace and plugin support is **experimental** and may change incompatibly
or be removed in a future release.

This file records target-client checks separately from deterministic import and
configuration tests. Generated configuration alone is not treated as an MCP
tool-call result.

## 2026-09-22

Fixture: `crates/yasm-marketplace/tests/fixtures/runtime-marketplace`, containing
a local stdio server whose `ping` tool returns `pong-yasm`.

An isolated PTY smoke run registered the catalog, deployed the package to
Codex, Claude, and Cursor, completed MCP initialization and discovery, called
the `ping` tool, received `pong-yasm`, and removed the managed outputs.

| Target | Client version | Result |
| --- | --- | --- |
| Codex | `codex-cli 0.154.0` | `codex mcp get yasm-ping --json` loaded the generated global TOML entry with the rendered immutable package path. An authenticated model tool call was not run. |
| Claude Code | `2.1.273` | `claude mcp get yasm-ping` loaded the generated global JSON entry and reported the deterministic server as connected. An authenticated model tool call was not run. |
| Cursor | unavailable | The installed launcher reported that no Cursor IDE installation was present; editor/agent discovery and a model tool call remain to be run in a Cursor environment. |

The ordinary test suite starts the same deterministic server, completes MCP
initialization and tool discovery, calls `tools/call` for `ping`, and verifies
the `pong-yasm` response. This proves the fixture independently of public
services, but it does not replace the outstanding authenticated call through
each target client.

Re-run target checks in isolated homes/projects and record the exact client
versions here. For a complete acceptance run, call `ping` through each target,
then separately cover HTTP, authentication references, target approval/trust,
and reload behavior.

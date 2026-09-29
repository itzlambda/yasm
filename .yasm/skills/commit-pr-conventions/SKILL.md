---
name: commit-pr-conventions
description: Write consistent Conventional Commit subjects and pull request titles for yasm. Use when creating or amending commits, drafting or opening PRs, or updating PR titles and descriptions in this project.
---

# Commit and PR Conventions

Use Conventional Commits for every agent-authored commit subject and pull request title:

```text
type: summary
type(scope): summary
```

- Use a lowercase type from the table below. New features use `feat`.
- Scope is optional. When useful, use a short lowercase component name in parentheses, such as `cli`, `core`, or `providers`. Omit it for changes spanning the project without one clear component.
- Write a concise, specific summary in the imperative (for example, `add`, `fix`, or `remove`), starting with lowercase and without a trailing period.
- Choose the type from the actual change, not the branch name, task label, or agent name. Avoid vague summaries such as `update code` or `misc fixes`.
- Mark breaking changes with `!` before the colon, for example `feat(cli)!: rename the remove command`, and explain the breaking change and migration in the commit body or PR description.

| Type | Use for |
| --- | --- |
| `feat` | New features or capabilities |
| `fix` | Bug fixes |
| `refactor` | Code restructuring without a feature addition or bug fix |
| `perf` | Performance improvements |
| `docs` | Documentation-only changes |
| `test` | Test additions or corrections |
| `build` | Build tooling, packaging, or dependency changes |
| `ci` | CI workflow changes |
| `style` | Formatting-only changes with no behavior change |
| `chore` | Maintenance that does not fit another type |
| `revert` | Reverting a previous change |

Examples that work as either commit subjects or PR titles:

```text
feat(cli): add skill search
fix(core): skip git directories during skill discovery
refactor(providers): simplify provider registration
docs: document project-level skill installation
```

Before committing, check that the subject describes the staged changes. Before opening or updating a PR, check that its title describes the complete PR diff and its primary outcome; do not simply copy the latest commit subject. Update the title and description if the PR scope changes. Keep the PR description in normal prose covering the problem, resulting behavior, and relevant validation; the prefix convention applies to the title.

Follow the repository's quality gate in `AGENTS.md` before publishing. This skill governs naming and does not itself authorize committing, pushing, or opening a PR.

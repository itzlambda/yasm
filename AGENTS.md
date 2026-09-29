We are building a CLI which can be used to manage skills for user globally or for a project specifically.

# Agent Guidelines

## Commit Messages and Pull Request Titles

Read and follow [commit-pr-conventions](.agents/skills/commit-pr-conventions/SKILL.md) when creating or amending commits, or drafting, opening, or updating pull requests.

## CLI Design Invariants

- Anything possible through an interactive TTY flow must also be possible in a non-TTY context by passing explicit CLI arguments.
- TTY mode may prompt for missing choices.
- Non-TTY mode must never prompt. It should fail with an actionable message that names the missing flag or argument.
- Keep static CLI choices in typed `clap` values where possible so invalid input gets consistent help and valid-value suggestions.
- Keep runtime validation for dynamic values such as installed skill IDs and discovered skill names, and include available candidates in those errors.

## Required Quality Gate

- Run `just lint` after the final change and before pushing a branch, opening or updating a pull request, or pushing directly to `main`.
- The gate does not need to run after every commit. Run it against the final state that will be pushed.
- If anything changes after a successful run, run `just lint` again before pushing.
- Do not push or open a pull request while the gate is failing. Fix the failure and rerun the complete gate.

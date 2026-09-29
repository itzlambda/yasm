---
name: issue-to-pr
description: Take a GitHub issue through architectural review, implementation, validation, commit, and pull request. Use when asked to implement an issue through a PR, not merely discuss or draft one.
---

# Issue to PR

Use the issue number or URL supplied by the user. If none is provided or clearly established in context, ask which issue to work on.

1. Read the issue, comments, linked proposals, repository instructions, and relevant code, tests, and history. Establish the intended behavior and acceptance criteria; treat any suggested fix as a proposal to verify.
2. Check the proposal against the actual cause, existing architectural boundaries, public contracts, and adjacent behavior. Prefer a focused change using established patterns over introducing unnecessary abstractions or unrelated refactoring.
3. If the proposal is sound, briefly explain why and proceed. If it is flawed, or no solution is proposed, present a concrete recommended approach with affected components, behavior, tradeoffs, and validation plan. Wait for the user's approval before implementing that alternative. Reuse approval already given in the conversation; routine implementation details do not require another approval.
4. Implement on a task branch, preserving unrelated user changes. Add appropriate regression coverage and update affected documentation. Review the complete diff against the issue and approved approach.
5. Run the repository's required quality gate against the final state, plus relevant checks. Fix failures before publishing; rerun required checks after subsequent changes. If blocked by missing access or an unresolved requirement, report the blocker without claiming completion.
6. When the user's request authorizes the full issue-to-PR workflow, commit the task changes, push the branch, and open a PR without asking again. Follow repository commit and PR conventions. Describe the problem, resulting behavior, and validation; reference the issue with `Closes #<number>` only when fully addressed. Check for an existing task PR before creating a duplicate. Do not merge it or post separate issue comments unless requested.

Finish with the PR link, a brief description of the fix, and validation results. If the user requested only analysis or a plan, stop at that scope; this skill does not grant permission to publish by itself.

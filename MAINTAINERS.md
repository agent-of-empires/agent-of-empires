# Maintainers

| Area | Team | Owner | Reviewers |
| --- | --- | --- | --- |
| Web dashboard, plugin pages, plugin API | `web` | @Seluj78 | @cwrau |
| ACP, structured view, composer, attachments | `acp` | @cwrau | @Seluj78 |
| Daemon, session lifecycle, worktrees, process, containers, build | `runtime` | @jerome-benoit | @njbrake |
| Hooks, agent integrations, OMP, tmux | `integrations` | @athal7 | @jerome-benoit |
| Security, stored data, supply chain, governance | `protected` | @njbrake | @Seluj78, @jerome-benoit |
| Releases | `release` | @njbrake | @Seluj78 |

[CODEOWNERS](.github/CODEOWNERS) maps paths to these `@agent-of-empires`
teams; each auto-assigns one reviewer. Only @njbrake and @Seluj78 cut
[releases](docs/development/releases.md). @njbrake sets project direction.

## Merging

A PR merges with green CI and one approval from a maintainer other than its
author. The author or the approver squash-merges it. No PR needs @njbrake by
default.

Protected and release paths also need an approval from their team.

## Reviewing

- One reviewer per PR: the one GitHub assigned. Other maintainers stay off
  unless asked, and comment only to flag a P1.
- Assign yourself when you start, so others can see the PR is taken.
- If you cannot get to a PR, re-request another team member instead of
  letting it sit.
- The same reviewer sees a PR through to merge.
- A PR spanning areas gets one reviewer, plus only the protected or release
  approvals it requires.

Review with the [rubric](CONTRIBUTING.md#review-rubric).

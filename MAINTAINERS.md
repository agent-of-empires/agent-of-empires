# Maintainers

| Maintainer | Areas |
| --- | --- |
| [@njbrake](https://github.com/njbrake) | Project direction; security, releases, governance |
| [@Seluj78](https://github.com/Seluj78) | Web dashboard, plugin pages, plugin API; security, releases, governance |
| [@cwrau](https://github.com/cwrau) | ACP, structured view, composer, attachments |
| [@jerome-benoit](https://github.com/jerome-benoit) | Daemon, session lifecycle, worktrees, process, containers, build |
| [@athal7](https://github.com/athal7) | Hooks, agent integrations, OMP, tmux |

[CODEOWNERS](.github/CODEOWNERS) maps paths to their owners. Only @njbrake and
@Seluj78 cut [releases](docs/development/releases.md).

## Merging

A PR merges with green CI and one approval from a maintainer other than its
author. The author or the approver squash-merges it. No PR needs @njbrake by
default.

Protected paths (auth, login, rate limiting, tunnel, push, migrations, session
storage, `Cargo.*`, `flake.*`, `.github/`, `scripts/`, release configuration,
and repository guidance files) also need their code owner's approval.

## Reviewing

Maintainers review PRs in their area using the
[review rubric](CONTRIBUTING.md#review-rubric).

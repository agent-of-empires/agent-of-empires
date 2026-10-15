# Maintainers

| Maintainer | Areas |
| --- | --- |
| [@njbrake](https://github.com/njbrake) | Project direction; protected paths |
| [@Seluj78](https://github.com/Seluj78) | Web dashboard, plugin pages, plugin API |
| [@cwrau](https://github.com/cwrau) | ACP, structured view, composer, attachments |
| [@jerome-benoit](https://github.com/jerome-benoit) | Daemon, session lifecycle, worktrees, process, containers, build |
| [@athal7](https://github.com/athal7) | Hooks, agent integrations, OMP, tmux |

[CODEOWNERS](.github/CODEOWNERS) maps paths to an owner and a second reviewer.

## Merging

A PR merges with green CI and one approval from a maintainer other than its
author. The author or the approver squash-merges it. No PR needs @njbrake by
default.

Protected paths (auth, login, rate limiting, tunnel, push, migrations, session
storage, `Cargo.*`, `flake.*`, `.github/`, `scripts/`, release configuration,
and repository guidance files) also need their code owner's approval.

## Reviewing

Each maintainer reviews PRs in their area within about 2 business days, using
the [review rubric](CONTRIBUTING.md#review-rubric).

## Design first

Agree a short ADR in [docs/adr/](docs/adr/) or a design issue with the area
owner before writing code for:

- a change adding more than about 1,500 lines;
- a new public API or plugin manifest slot;
- a new subsystem.

Label direction, scope and tie-break questions `needs-decision`; @njbrake
decides them.

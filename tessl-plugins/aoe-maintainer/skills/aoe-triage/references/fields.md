# Triage fields

The vocabulary and scales the `aoe-triage` skill classifies with. The live
label list (`gh label list`) and the live board fields (`gh project
field-list`) win over this page when they differ.

## Labels

Use only labels that `gh label list` returns. The skill adds labels from this
page and nothing else.

**Priority**: exactly one.

| Label | Meaning |
| ----- | ------- |
| `P0` | Stop the bleeding: a regression, data loss, a crash on a common path, or an onboarding blocker. |
| `P1` | Current focus: a real bug most users can hit, or work a maintainer has said is next. |
| `P2` | Real but not now. The default. |

An issue that already has any `P0` to `P3` label has a priority. The skill
never adds `P3`.

**Type**: exactly one. The issue templates set it, so most new issues already
have one.

| Label | Use for |
| ----- | ------- |
| `bug` | Something that worked, or is documented to work, does not. Includes flaky tests and regressions. |
| `enhancement` | A new capability, a UX improvement, a refactor, docs work, or an epic. |
| `question` | A usage question with no defect or request in it. |

**Area**: exactly one, from where the change would land. Read the issue's
file paths, error text, commands and screenshots' captions, then confirm with
`grep` in the clone when the surface is not named.

| Label | Lands in |
| ----- | -------- |
| `area:cli` | `src/cli/`, `aoe <subcommand>` behavior and output. |
| `area:tui` | `src/tui/`, the terminal UI. |
| `area:web` | `web/`, the web dashboard on desktop. |
| `area:mobile` | The web dashboard on a phone: touch, mobile toolbar, safe-area, mobile drawers. |
| `area:server` | `src/server/`, the HTTP and WebSocket API behind `aoe serve`: endpoints, auth, profile and settings writes. |
| `area:agents` | `src/agents.rs`, `src/acp/`, `src/hooks/`, status detection, agent and plugin integrations. |
| `area:docker` | `src/containers/`, `docker/`, sandboxing and images. |
| `area:core` | `src/session/`, `src/daemon/`, `src/tmux/`, `src/git/`, `src/process/`, `src/plugin/`: session, storage, worktree and lifecycle logic shared by every surface. |
| `area:infra` | CI, release, packaging, build, test harness, dependencies. |
| `area:docs` | `docs/`, `website/`, README. |

When two areas fit, pick the one the fix would mostly touch. Never add a
second area.

**Add-ons**: add only when the issue text or code evidence shows it.

| Label | When |
| ----- | ---- |
| `blocked:upstream` | The fix waits on a named external project (an adapter, a crate, tmux). |
| `epic` | The issue tracks several other issues and is not itself one change. |
| `good first issue` | One small, well-located change with the location named in the issue. |
| `plugin` | The request fits a plugin better than core. |
| `duplicate` | Only with the duplicate comment, see the skill. |

## Board fields

Board: `Agent of Empires`, project number `1`, owner `agent-of-empires`. It
holds issues from every repository in the organization. Resolve field and
option IDs by name from `gh project field-list 1 --owner agent-of-empires
--format json`; never reuse IDs from an earlier run.

| Field | Value |
| ----- | ----- |
| `Status` | `Ready` for `P0` and `P1`, `Backlog` for `P2` and `P3`. `Blocked` only with `blocked:upstream`. |
| `Priority` | The priority label. No value for `P3`, which the field lacks. |
| `Size` | From the scale below. |
| `Roadmap state` | `enhancement` issues only: `Next` when Status is `Ready`, `Later` when `Backlog`. Leave it empty on `bug` and `question`. Never `Now`. |

**Size**

| Size | Scope |
| ---- | ----- |
| `XS` | One line or one file, trivial. |
| `S` | One component, a few hours. |
| `M` | Several files, half a day to a day. |
| `L` | Cross-cutting, several days. |
| `XL` | Epic-shaped, a week or more. |

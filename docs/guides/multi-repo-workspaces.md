# Multi-Repo Workspaces

Run one session across several git repositories. Each repo gets its own worktree on a shared branch name, all rooted under one workspace directory, attached to one tmux session. Use it when a unit of work touches more than one repo and you want one agent driving all of them.

For fixed sibling repos that rarely change, an [`on_create` hook](repo-config.md) is simpler than a workspace.

## Quick start

Register the repos once (see [the registry](#the-project-registry)), then start a session against them:

```bash
aoe project add /path/to/backend
aoe project add /path/to/frontend

aoe add /path/to/backend --project frontend --project shared-lib \
  -w feat/auth-rewrite -b
```

TUI: in the new-session dialog, focus **Extra Repos**, press `Ctrl+R`, and pick the registered projects. Web: pick a primary repo, then click projects in the **Extra repos** picker, or paste a path.

The session starts in the workspace root with the worktrees as siblings:

```
~/aoe-workspaces/feat-auth-rewrite/
├── backend/      ← branch feat/auth-rewrite
├── frontend/     ← branch feat/auth-rewrite
└── shared-lib/   ← branch feat/auth-rewrite
```

The agent navigates between them with ordinary `cd` and git commands; AoE imposes no cross-repo orchestration.

Worktree creation runs concurrently, so the wall-clock cost is roughly the slowest repo rather than the sum. A failing post-checkout hook surfaces as a warning and does not abort the workspace, as for [single-repo worktrees](worktrees.md#warnings-during-create).

### Per-repo base branches

Each repo's branch forks from `--base-branch` by default. `--repo-base <repo>=<branch>` overrides one, so a workspace can start with one repo on `develop` and another on its own epic branch:

```bash
aoe add /path/to/backend --project frontend \
  -w feat/auth-rewrite -b --base-branch develop --repo-base frontend=epic/checkout
```

`<repo>` is the repo's directory name or a path you passed to `--repo`; an unmatched name is an error rather than a silent fallback. The web wizard has the same field per repo. The base each repo forked from becomes its default diff comparison ref (see [Base override](diff-view.md#base-override)), changeable later with `aoe session set-base --repo <name>`.

## Adding a repo to an existing session

```bash
aoe session add-project <session> frontend
```

Web and TUI: **Add project** on the session's right-click menu, or the command palette. Afterwards the session is indistinguishable from one created with `--project`: both repos sit under one workspace directory and `aoe list --json` reports both in `workspace_repos`.

| Session was | What happens | Working directory |
|---|---|---|
| A multi-repo workspace | The new repo's worktree joins the existing workspace | Unchanged |
| A worktree session | A workspace is created and the session's worktree moves into it, so uncommitted work travels | Moves |
| An in-place session | A workspace is created with a *fresh* worktree of the session's repo | Moves |

That last row is why AoE never touches your own checkout: it creates a worktree rather than adopting the directory you work in. Uncommitted work there would be left behind, so attaching to an in-place session with a dirty checkout is refused. Commit or stash first.

The session's branch name is a suggestion: if the added repo lacks that branch, AoE creates it from that repo's own base. If it already exists the attach is refused, since a same-named branch elsewhere can hold unrelated commits; pass `--attach-existing-branch` (or tick the box in the web modal) to check it out as-is, and AoE then leaves that branch alone when the session is deleted.

Unless the session is already a workspace, its directory moves, so it is stopped for the move and restarted; a structured session resumes the same conversation. Attaching is refused mid-turn, and everything that can refuse it is checked before anything is stopped, so a refusal never costs you a running session. Scratch sessions cannot be attached to.

A sandboxed session has its container recreated, since bind mounts are fixed at creation and the container mounts the common ancestor of the workspace and every repo. Build caches go with the container under the default [volume ignores strategy](sandbox.md#volume-ignores); under `"named"` they are keyed on their container path and survive an attach that leaves that path alone. Attaching a repo from outside the current common ancestor moves every mount, so those caches start cold and the volumes they leave behind need removing by hand.

## The project registry

Saved repo paths the pickers draw from, in two scopes:

| Scope | File | Visibility |
|---|---|---|
| Global | `<app_dir>/projects.json` | Every profile |
| Profile | `<app_dir>/profiles/{profile}/projects.json` | Only that profile |

```bash
aoe project list [--scope global|profile] [--json]   # merged by default
aoe project add /path/to/repo [--name shortname] [--scope profile]
aoe project remove backend                           # by name or canonical path
```

`aoe project add` defaults to global, or to the profile when you pass `-p <profile>`. Adding a path that exists in another scope is an error unless you pass `--allow-override`, which lets a profile entry shadow the global one and win in merged views.

A saved project appears in the pickers whether or not it has sessions. **Pinning** is separate: it keeps the project's header visible in the sidebar even with zero sessions. So unpinning does not delete a project; only `aoe project remove` (or the **Remove** action) deletes the registry entry.

Pin from the TUI project view (press `g`, pick Project grouping, then `p` on a header, or use its right-click menu) or from the web sidebar's project header menu. Pinning registers the repo globally if it was not saved yet, marks the header with `◆`, and sorts pinned-but-empty projects below active repositories. The pin lives in the registry, so it shows up on the other surface too.

## Where workspaces appear

- **TUI**: press `b` from the home view (or the command palette's "New session from saved project") for a filterable picker over the merged registry. Selecting one pre-fills the new-session dialog.
- **Web**: the sidebar's [Projects section](web/dashboard.md#projects) manages the registry, and the wizard surfaces it as toggleable chips with a free-text input for unregistered paths. Multi-repo sessions are bucketed into a single **Multi-repo** group at the bottom of the sidebar, each row showing a chip per repo. Read-only servers hide the destructive controls.
- **CLI**: `--repo` (a literal path) and `--project` (a registered name) may be mixed, and the builder rejects duplicate repo names, so the same repo via two paths is a hard error. `aoe list --json` carries `workspace_repos`, empty for single-repo sessions.

## Limitations

- One branch name per workspace: every repo gets the same `-w <branch>`.
- The agent cannot add a repo to its own session; you add one, without losing the conversation.
- No saved workspace templates, and no per-repo PR tracking.

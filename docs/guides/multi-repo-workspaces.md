# Multi-Repo Workspaces

Run one session across several git repositories. Each repo gets its own worktree on a shared branch name, all rooted under one workspace directory, attached to one tmux session. Use it when a unit of work touches more than one repo and you want one agent driving all of them.

For fixed sibling repos that rarely change, an [`on_create` hook](repo-config.md) is simpler than a workspace.

## Quick start

Register repos once, then start a session:

```bash
aoe project add /path/to/backend
aoe project add /path/to/frontend
aoe project add /path/to/shared-lib

aoe add /path/to/backend --project frontend --project shared-lib \
  -w feat/auth-rewrite -b
```

TUI: in the new-session dialog (`n`), focus **Extra Repos**, press `Ctrl+R`, and select registered projects. Web: select a primary repo and use **Extra repos**, or enter a path.

### Per-repo base branches

Each repo forks from `--base-branch` by default. Override one with `--repo-base <repo>=<branch>`:

```bash
aoe add /path/to/backend --project frontend \
  -w feat/auth-rewrite -b --base-branch develop --repo-base frontend=epic/checkout
```

`<repo>` is the repo directory name or a `--repo` path; unmatched names fail rather than falling back. The web wizard exposes the same per-repo field. Each recorded base becomes its repo's diff comparison ref, changeable with `aoe session set-base --repo <name>`; see [Base override](diff-view.md#base-override).

Worktree creation runs concurrently. A failed post-checkout hook leaves the completed checkout usable and surfaces a warning rather than aborting the workspace. See [Post-checkout hooks](worktrees.md#post-checkout-hooks).

### Workspace layout

The session starts in the workspace root with all the worktrees as siblings:

```
~/aoe-workspaces/feat-auth-rewrite/
├── backend/      ← branch feat/auth-rewrite
├── frontend/     ← branch feat/auth-rewrite
└── shared-lib/   ← branch feat/auth-rewrite
```

The agent navigates between them with ordinary `cd` and git commands; AoE imposes no cross-repo orchestration.

### Adding a repo to an existing session

Add a repository without recreating the session:

```bash
aoe session add-project <session> frontend
```

Web and TUI: choose **Add project** on the session's context menu, or use the command palette. The result is the same workspace shape as `--project`, reported in `aoe list --json` as `workspace_repos`.

What that means for the session depends on the shape it started in:

| Session was | What happens | Working directory |
|---|---|---|
| A multi-repo workspace | The new repo's worktree joins the existing workspace | Unchanged |
| A worktree session | A workspace is created and the session's worktree moves into it, so uncommitted work travels | Moves |
| An in-place session | A workspace is created with a *fresh* worktree of the session's repo | Moves |

That last row is why AoE never touches your own checkout: it creates a worktree rather than adopting the directory you work in. Uncommitted work there would be left behind, so attaching to an in-place session with a dirty checkout is refused. Commit or stash first.

The session's branch name is a suggestion: if the added repo lacks that branch, AoE creates it from that repo's own base. If it already exists the attach is refused, since a same-named branch elsewhere can hold unrelated commits; pass `--attach-existing-branch` (or tick the box in the web modal) to check it out as-is, and AoE then leaves that branch alone when the session is deleted.

Unless the session is already a workspace, its directory moves, so it is stopped for the move and restarted. A known conversation bound to the old directory blocks the move; other sessions may attempt to resume their stored ID, but continuity is not guaranteed. Attaching is refused mid-turn. Everything that can refuse the attach is checked before stopping; if conversion later fails, AoE attempts to restore the worker.

Scratch sessions cannot accept attached repositories; they have no repository of their own.

A sandboxed session has its container recreated, since bind mounts are fixed at creation and the container mounts the common ancestor of the workspace and every repo. Build caches go with the container under the default [volume ignores strategy](sandbox.md#volume-ignores-strategy); under `"named"` they are keyed on their container path and survive an attach that leaves that path alone. Attaching a repo from outside the current common ancestor moves every mount, so those caches start cold and the volumes they leave behind need removing by hand.

## The project registry

Saved repo paths the pickers draw from, in two scopes:

| Scope | File | Visibility |
|---|---|---|
| Global | `<app_dir>/projects.json` | Every profile |
| Profile | `<app_dir>/profiles/{profile}/projects.json` | Only that profile |

See [File Locations](configuration.md#file-locations) for `<app_dir>`. `aoe project add` defaults to global, or to the selected profile with `-p <profile>`; `--scope global` or `--scope profile` overrides that choice. A path already registered in another scope is refused unless `--allow-override` lets a profile entry shadow the global one.

Saved projects appear in pickers even without sessions. **Pinning** keeps a project header visible when empty; unpinning leaves the registration intact. Only **Remove** or `aoe project remove` deletes the entry.

Pin from the TUI project view (`g`, Project grouping, then `p` on a header or its context menu), or the web project header's actions menu. Pinning registers an unsaved repo globally and marks it with `◆`. Empty pinned web projects sort below active repositories and retain their **New session** action. Pins live in the registry and appear on the other surface when refreshed.

## CLI

Use `aoe project list`, `aoe project add <path>` and `aoe project remove <name-or-path>` to manage registrations. See the [CLI Reference](../cli/reference.md) for scope, naming, override and JSON flags.

`--repo` accepts literal paths and can be mixed with registered `--project` names. Duplicate repo names, including the same repo reached through two paths, are refused. `aoe list --json` reports `workspace_repos`, empty for single-repo sessions.

## TUI

From the home view, press `b` (or `B` with strict hotkeys) to open a filterable picker over the merged registry. Selecting a project opens the new-session dialog pre-filled with that project's path. The same action is available from the `Ctrl+K` command palette ("New session from saved project"). With no registered projects, the picker is replaced by a "No Projects" prompt pointing at `aoe project add`.

## Web Dashboard

Use the sidebar's **Projects, no sessions** section to add a Project, edit its default base branch, or remove its registration. Unpin a project first if it is displayed as a persistent repository header. Changing a registration's name, path, or scope requires removing and adding it again. Read-only servers hide mutation controls.

Remove targets the registrations represented by that project row, using the profile captured when the action starts. It does not request deletion from unrelated profiles. Global registrations are shared, so removing one changes every profile's merged view.

Saved-project suggestions in the new-session wizard follow the selected profile. An open project form keeps its original profile even if the daemon's default profile changes. The picker for adding a repository to an existing session uses that session's profile. Unregistered paths can still be entered directly.

Multi-repo sessions are bucketed into a single **Multi-repo** group at the bottom of the sidebar, regardless of which repo was chosen as the primary. Each session row shows a chip per repo under the title.

## Limitations

- **One branch name per workspace**: every repo gets the same `-w <branch>` value.
- **No agent-driven repo pull-in**: the agent cannot add a repo to its own session; attaching may restart the agent without resuming its conversation.
- **No saved workspace templates**: each session picks the repo set fresh.
- **No per-repo PR tracking**: coordinated PR workflow happens outside AoE.

## Related

- [Worktrees Reference](worktrees.md): how the per-repo worktrees are created.
- [Repository Configuration & Hooks](repo-config.md): `on_create` hooks for fixed sibling repos that don't need a registry entry.
- [CLI Reference](../cli/reference.md): full `aoe project` and `aoe add --project` flag listing.

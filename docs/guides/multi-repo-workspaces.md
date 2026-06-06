# Multi-Repo Workspaces

Run a single AoE session across several git repositories at once. Each repo gets its own worktree on a shared branch name, all rooted under one workspace directory, attached to one tmux session.

Use this when a unit of work, a feature, a bug fix, an investigation, touches more than one repo and you want one agent driving all of them, not N agents you have to mentally reconcile.

## When to Use

| Scenario | Multi-repo? |
|---|---|
| Bug spans backend and frontend repos | Yes |
| Refactor across an OSS core and a private wrapper | Yes |
| Feature limited to a single repo | No, regular session |
| Investigating logs that touch many repos | Yes, agent picks the relevant ones |
| OSS core is pinned and rarely changes | Use [`on_create` hooks](repo-config.md) instead |

## Quick Start

### 1. Register your repos once

```bash
aoe project add /path/to/backend
aoe project add /path/to/frontend
aoe project add /path/to/shared-lib
```

`aoe project list` shows what is registered.

### 2. Start a multi-repo session

CLI:

```bash
aoe add /path/to/backend \
  --project frontend \
  --project shared-lib \
  -w feat/auth-rewrite -b
```

TUI: open the new-session dialog (`n`), enter the worktree branch, focus the **Extra Repos** field, press `Ctrl+R`, and pick the registered projects you want to include.

Web: `+ New session`, pick a primary repo, then click registered projects in the **Extra repos** picker (or paste a path with the free-text input).

Worktree creation across the repos in a workspace runs concurrently, so wall-clock time is roughly that of the slowest single repo rather than the sum (network-bound `git fetch` and `git submodule update` dominate). If any repo's post-checkout hook fails after `git worktree add` has already checked out the branch, the workspace is still created and the hook output is surfaced as a warning. See [Post-Checkout Hooks](worktrees.md#post-checkout-hooks) for details.

### 3. The agent sees one workspace

The session starts in the workspace root with all the worktrees as siblings:

```
~/aoe-workspaces/feat-auth-rewrite/
├── backend/      ← branch feat/auth-rewrite
├── frontend/     ← branch feat/auth-rewrite
└── shared-lib/   ← branch feat/auth-rewrite
```

The agent navigates between them like any normal multi-repo working tree. Use `cd` and standard git commands; AoE does not impose any cross-repo orchestration.

## The Project Registry

Saved repo paths the multi-repo pickers draw from. Two scopes:

| Scope | File | Visibility |
|---|---|---|
| Global | `<app_dir>/projects.json` | Every profile |
| Profile | `<app_dir>/profiles/{profile}/projects.json` | Only that profile |

`<app_dir>` is `$XDG_CONFIG_HOME/agent-of-empires/` on Linux, `~/.agent-of-empires/` on macOS.

`aoe project add <path>` defaults to global; `aoe -p <profile> project add <path>` defaults to profile. Pass `--scope global` or `--scope profile` to override.

Adding a path that already exists in another scope is an error unless you pass `--allow-override`, which lets a profile entry shadow the global one (the profile entry then wins in merged views):

```bash
aoe project add /repo/foo                              # global
aoe -p other project add /repo/foo --allow-override    # profile shadows global
```

## CLI Reference

```bash
# List
aoe project list                       # merged (global + active profile)
aoe project list --scope global        # globals only
aoe project list --scope profile       # active profile only
aoe project list --json                # machine-readable

# Add
aoe project add /path/to/repo                          # global, name = basename
aoe project add /path/to/repo --name shortname        # custom display name
aoe project add /path/to/repo --scope profile         # profile-only
aoe project add /path/to/repo --allow-override        # shadow other-scope entry

# Remove
aoe project remove backend                # by name (case-insensitive)
aoe project remove /path/to/repo          # by canonical path
aoe project remove backend --scope profile

# Use in a session
aoe add /path/to/primary --project name1 --project name2 -w branch -b
aoe add /path/to/primary --repo /literal/path --project registered -w branch -b
```

`--repo` and `--project` may be mixed; the union is passed to the workspace builder. The builder rejects duplicate repo names, so the same repo via two paths is a hard error.

`aoe list --json` includes a `workspace_repos` array for each session; the array is empty for single-repo sessions.

## TUI

From the home view, press `b` (or `B` with strict hotkeys) to open a filterable picker over the merged registry. Selecting a project opens the new-session dialog pre-filled with that project's path. The same action is available from the `Ctrl+K` command palette ("New session from saved project"). With no registered projects, the picker is replaced by a "No Projects" prompt pointing at `aoe project add`.

## Web Dashboard

The Projects page (folder icon in the sidebar footer) is full CRUD over the registry: add, remove, switch scope, opt into `allow_override`. Read-only servers (`aoe serve --read-only`) hide the destructive controls.

The new-session wizard surfaces the registry as toggleable chips on the Project step. The free-text input still works for paths that aren't registered.

Multi-repo sessions are bucketed into a single **Multi-repo** group at the bottom of the sidebar, regardless of which repo was chosen as the primary. Each session row shows a chip per repo under the title.

## Limitations

- **One branch name per workspace**: every repo gets the same `-w <branch>` value.
- **No agent-driven repo pull-in mid-session**: to add a repo, start a new session.
- **No saved workspace templates**: each session picks the repo set fresh.
- **No per-repo PR tracking**: coordinated PR workflow happens outside AoE.

## Related

- [Worktrees Reference](worktrees.md) — how the per-repo worktrees are created.
- [Repository Configuration & Hooks](repo-config.md) — `on_create` hooks for fixed sibling repos that don't need a registry entry.
- [CLI Reference](../cli/reference.md) — full `aoe project` and `aoe add --project` flag listing.

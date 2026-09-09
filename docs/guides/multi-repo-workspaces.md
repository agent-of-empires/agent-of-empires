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

### 3. Give a repo its own base branch

Each repo's branch is forked from `--base-branch` by default. `--repo-base <repo>=<branch>` overrides that for one repo, so a workspace can start with one repo on `develop` and others on their own epic branches:

```bash
aoe add /path/to/backend \
  --project frontend \
  --project shared-lib \
  -w feat/auth-rewrite -b \
  --base-branch develop \
  --repo-base frontend=epic/checkout
```

`<repo>` is the repo's directory name or a path you passed to `--repo`; an unmatched name is an error rather than a silent fallback. The web wizard has the same thing as a base-branch field on each repo in the **Extra repos** picker. The base each repo was forked from is recorded, and it becomes that repo's default diff comparison ref (see [Base override](web/diff.md#base-override)); `aoe session set-base --repo <name>` changes it later, per repo.

Worktree creation across the repos in a workspace runs concurrently, so wall-clock time is roughly that of the slowest single repo rather than the sum (network-bound `git fetch` and `git submodule update` dominate). If any repo's post-checkout hook fails after `git worktree add` has already checked out the branch, the workspace is still created and the hook output is surfaced as a warning. See [Post-Checkout Hooks](worktrees.md#post-checkout-hooks) for details.

### 4. The agent sees one workspace

The session starts in the workspace root with all the worktrees as siblings:

```
~/aoe-workspaces/feat-auth-rewrite/
├── backend/      ← branch feat/auth-rewrite
├── frontend/     ← branch feat/auth-rewrite
└── shared-lib/   ← branch feat/auth-rewrite
```

The agent navigates between them like any normal multi-repo working tree. Use `cd` and standard git commands; AoE does not impose any cross-repo orchestration.

### 5. Add a repo to a session that already exists

When you get twenty minutes into a task and realize you also need another repo,
attach it instead of recreating the session:

```bash
aoe session add-project <session> frontend
```

Web: right-click the session in the sidebar, **Add project**. TUI: same action on
the right-click menu, or `Add project to this session` in the command palette.

Attaching converts the session into a real multi-repo workspace. Afterwards it is
indistinguishable from one created with `--project`: both repos sit side by side
under one workspace directory, and `aoe list --json` reports both in
`workspace_repos`. There is no second class of "attached" repo.

What that means for the session depends on the shape it started in:

| Session was | What happens | Working directory |
|---|---|---|
| A multi-repo workspace | The new repo's worktree is created in the workspace it already has | Unchanged |
| A worktree session | A workspace directory is created and the session's worktree is moved into it, so uncommitted work travels | Moves to the workspace |
| An in-place session | A workspace directory is created with a *fresh* worktree of the session's repo | Moves to the workspace |

The last row is why AoE never touches your own checkout: it creates a worktree of
your repo rather than adopting the directory you are working in. The trade is that
uncommitted work in that checkout would be left behind, so attaching to an
in-place session with a dirty checkout is refused. Commit or stash first.

The session's branch name is only a suggestion. If the added repo does not have
that branch, AoE creates it from that repo's own base branch. If it already
exists, the attach is refused, because a same-named branch in another repo can
hold unrelated commits: pass `--attach-existing-branch` (or tick the box in the
web modal) to check it out as-is. AoE then records that it did not create the
branch and leaves it alone when the session is deleted.

Unless the session is already a workspace, its working directory moves, so the
session is stopped for the move and started again afterwards. A structured
(ACP) session resumes the same conversation. Attaching is refused while the agent
is mid-turn; wait for the turn to finish or cancel it. Everything that can refuse
the attach is checked before anything is stopped, so a refusal never costs you a
running session.

Scratch sessions cannot be attached to: they have no repo of their own, only a
throwaway directory that deletion removes.

Sandboxed sessions have their container removed and recreated, since bind mounts
are fixed when the container is created and the container mounts the common
ancestor of the workspace and every repo. Build caches (`target/`,
`node_modules/`) go with the container under the default
[volume ignores strategy](sandbox.md#volume-ignores-strategy-macosvirtiofs). Under
`"named"` they are keyed on their container path, so they survive an attach that
leaves that path alone. Attaching a repo from outside the current common ancestor
moves every mount, so those caches start empty and the volumes they leave behind
have to be removed by hand.

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

### Saved projects versus pinned projects

A registered (saved) project is a registry entry: it shows in the Projects view and the new-session wizard's multi-select picker, whether or not it has any sessions. Pinning is a separate decision: it keeps the project's header visible in the sidebar / project view even with zero sessions. So a saved project is not forced into the sidebar, and unpinning a project does not delete it; it stays saved and only its sessionless header goes away. Removing a project (the Projects view's "Remove", or `aoe project remove`) is the one action that deletes the registry entry.

### Pinning a project from the TUI

In the TUI's project view (press `g` and pick Project grouping), a project header normally disappears once its last session is gone. Press `p` on a project header, or pick "Pin project" from its right-click menu, to pin it (registering the repo in the global registry if it is not already saved). A pinned project keeps its header (marked with a `◆`) even with zero sessions, so you can launch new work under it later. Press `p` again to unpin; the project stays saved (still in the picker), and its header drops from the view once it has no sessions.

### Pinning a project from the web dashboard

The web sidebar's project (repository) grouping mirrors the TUI. A pinned project shows a `◆` next to its name and keeps its header even with zero sessions; its `+` New session button launches work under that repo. Open a project header's actions menu (right-click, or the menu on the header) and choose "Pin project" to pin it (registering the repo, global scope, if needed) or "Unpin project" to clear the pin while keeping the saved project. Pinned-but-empty projects sort below your active repositories. The pin is stored in the registry, so a pin made in the TUI shows up here when the dashboard regains focus, and vice versa.

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

The new-session wizard surfaces the registry as toggleable chips in the Project section. The free-text input still works for paths that aren't registered.

Multi-repo sessions are bucketed into a single **Multi-repo** group at the bottom of the sidebar, regardless of which repo was chosen as the primary. Each session row shows a chip per repo under the title.

## Limitations

- **One branch name per workspace**: every repo gets the same `-w <branch>` value.
- **No agent-driven repo pull-in**: the agent cannot add a repo to its own session. You add one yourself, without losing the conversation, as described in [Add a repo to a session that already exists](#add-a-repo-to-a-session-that-already-exists).
- **No saved workspace templates**: each session picks the repo set fresh.
- **No per-repo PR tracking**: coordinated PR workflow happens outside AoE.

## Related

- [Worktrees Reference](worktrees.md) — how the per-repo worktrees are created.
- [Repository Configuration & Hooks](repo-config.md) — `on_create` hooks for fixed sibling repos that don't need a registry entry.
- [CLI Reference](../cli/reference.md) — full `aoe project` and `aoe add --project` flag listing.

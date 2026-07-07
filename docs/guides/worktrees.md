# Worktrees Reference

Reference documentation for git worktree commands and configuration in `aoe`.

For workflow guidance, see the [Workflow Guide](workflow.md).

## CLI vs TUI Behavior

| Feature | CLI | TUI |
|---------|-----|-----|
| Create new branch | Use `-b` flag | Always creates new branch |
| Use existing branch | Omit `-b` flag | "Attach to existing branch" toggle (TUI: `Ctrl+P`; web: in the session step under the branch field, inside the "Advanced" disclosure) |
| Branch validation | Checks if branch exists | None (always creates) |
| Pick a base branch | `--base-branch <name>` | `Base` field in `Ctrl+P` overlay |

## CLI Commands

```bash
# Create worktree session (new branch, branched off the repo default)
aoe add . -w feat/my-feature -b

# Create worktree session (new branch, branched off a specific base)
aoe add . -w hotfix-1 -b --base-branch release-1.2

# Attach to an existing branch + worktree (or check out the branch into a
# new worktree if no worktree exists yet). The `-b` flag is what flips
# between "create a new branch" and "attach"; omitting it = attach.
aoe add . -w feat/my-feature

# List all worktrees
aoe worktree list

# Show session info
aoe worktree info <session>

# Find orphaned worktrees
aoe worktree cleanup

# Remove session (prompts for worktree cleanup)
aoe remove <session>

# Remove session and delete worktree
aoe remove <session> --delete-worktree
```

`--base-branch` only matters with `--new-branch` / `-b`. The base is
resolved against the remote first, then against a local branch with
that name, so passing a teammate's not-yet-fetched branch works
without a manual `git fetch`. When omitted, the new branch is based
on the repository's default branch (`main`/`master`).

Remote selection scores every configured remote (not just `origin`),
for both the autodetected default branch (issue \#1029) and an
explicit `--base-branch` (issue \#1511). In a fork plus `upstream`
layout where `upstream/main` is ahead of `origin/main`, aoe fetches
and branches off `upstream/main` even when you typed `main` into the
wizard's base-branch field. Ties break in favor of `origin` so the
historical single-remote behavior still applies when there is no
freshness signal.

## TUI Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `n` | New session dialog |
| `Tab` | Next field |
| `Shift+Tab` | Previous field |
| `Enter` | Submit and create session |
| `Esc` | Cancel |

In the TUI, enable the Worktree checkbox to create a new branch and worktree. By default, the worktree name is derived from the session title. Press `Ctrl+P` on the Worktree field to set an explicit `Name`, attach to an existing branch, pick a `Base` branch the new branch is based on (defaults to the repo default), or configure extra repos. `Ctrl+P` on the `Base` field opens a branch picker over local and remote-tracking branches.

The web dashboard's new-session wizard folds the worktree controls behind the single "More options" disclosure, leaving only the project picker, session title, and agent choice visible by default. Inside More options, a "Base branch" disclosure beneath the worktree name input shows a typeahead populated from local + remote branches via `GET /api/git/branches?include_remote=true`. The same section also exposes an "Attach to existing branch" toggle that flips the request from "create new branch" to "attach to whichever branch is named": when on, the server re-uses any existing worktree for that branch and otherwise checks the branch out into a new worktree. Mirrors the TUI / CLI behavior (CLI: omit `-b`). See #969 and #1514.

## Tying the Title and Worktree Directory

By default a worktree session's title and its worktree directory name stay tied: renaming the session moves the directory to match, and a new session's directory leaf is derived from its title (so "Auth refactor" lands in `.../auth-refactor` rather than a random codename). This is controlled by the `session.tie_workdir_to_name` setting (default `true`), which applies only to aoe-managed worktree sessions. Non-worktree (scratch, plain tmux) and attached worktree sessions ignore it.

```toml
[session]
tie_workdir_to_name = true
```

When tied:

- Renaming a session (TUI rename, web inline rename, `aoe session rename`, or `PATCH /api/sessions/{id}`) moves the worktree directory to the title's path-safe slug first, then sets the title only if the move succeeds, so the two cannot drift on a partial failure.
- The git branch is never swept in by a title rename by default. To rename it too, check "Also rename git branch" in the TUI rename dialog, pass `--rename-branch` to `aoe session rename`, or send `rename_branch: true` to the PATCH. It stays opt-in because a branch may carry an upstream or an open PR; the TUI toggle warns when the branch tracks a remote, since the remote branch (and any open PR) won't follow the local rename.
- The session must be stopped first. Moving the directory of a running worktree is unsafe, so a tied rename of a running session is refused with a clear message. Stop the session, or disable the setting, to relabel it freely.
- Naming collapses into the single rename action: the standalone "edit workdir name" affordance is hidden (TUI and web) and the standalone CLI / REST workdir-name edit is rejected, since the directory now follows the title.

Toggle the setting off (TUI settings, web settings, or the toml above) to relabel sessions freely while running and to edit the directory name independently of the title.

## Editing the Workdir Name After Creation

When `session.tie_workdir_to_name` is **off**, a worktree session's workdir (worktree directory) name is edited independently of its title. The worktree directory is moved in place via `git worktree move`, keeping its parent directory and swapping only the final path component (the new name's path-safe slug). Renaming the underlying git branch is opt-in, since a session may already have meaningful work or an upstream on its branch.

This supports only sessions whose worktree is aoe-managed (`worktree_info.managed_by_aoe = true`), and the session must not be running; otherwise you get a clear validation error and no change is made. The session title is left untouched.

| Surface | How |
|---------|-----|
| CLI | `aoe session set-worktree-name <session> --name <new-name>` (add `--rename-branch` to also rename the git branch) |
| TUI | Select the session, press `W` (or open the command palette and pick "Edit worktree workdir name"). Toggle "Also rename git branch" in the dialog. |
| Web | Right-click the session row, choose "Edit workdir name", enter a name, and optionally check "Also rename git branch". |
| REST | `PATCH /api/sessions/{id}/worktree-name` with `{ "name": "<new-name>", "rename_branch": <bool> }` |

The new directory and branch persist across reload and restart. See #1723 and #1927.

## Configuration

```toml
[worktree]
enabled = false
path_template = "../{repo-name}-worktrees/{branch}"
bare_repo_path_template = "./{branch}"
auto_cleanup = true
delete_branch_on_cleanup = false
init_submodules = true
```

Use `[session] row_tag = "branch"` to show worktree and workspace branch tags in
the TUI session list, or `"none"` to hide suffix metadata next to session names.

### Skipping submodule init

`init_submodules = false` skips the `git submodule update --init --recursive` step that runs after `git worktree add` when the checkout contains a `.gitmodules` file. Useful for repos that vendor deep submodule trees (e.g. OpenROAD-flow-scripts, llvm-project, chromium) where every new session would otherwise sit in `Creating…` for minutes while submodules clone. Per-invocation override on the CLI: `aoe add --worktree <branch> --no-submodules`.

On the delete side, aoe runs `git submodule deinit -f --all` before `git worktree remove` for any worktree with `.gitmodules`, so the panic-button `Force` checkbox is not required just because the worktree has submodules. If git still refuses (e.g. a partially-broken submodule), aoe falls back to clearing `<main>/.git/worktrees/<name>/modules/` and pruning the stale entry manually.

### Trashing relocates the worktree

Moving a worktree session to the trash (rather than purging it) relocates its worktree out of the active worktree dir into a sibling `.aoe-trash/<session-id>` holding directory via `git worktree move`, so trashed sessions stop cluttering the active checkouts. The worktree stays a live checkout, so previewing a trashed session still works. Restoring the session moves the worktree back to its original path; if that path is now occupied, the restore is refused so nothing is overwritten. Purging a trashed session removes the worktree from the holding dir. Sessions trashed before this behavior existed are relocated the next time the daemon starts or the TUI loads.

### Template Variables

| Variable | Description |
|----------|-------------|
| `{repo-name}` | Repository folder name |
| `{branch}` | Branch name (slashes converted to hyphens) |
| `{session-id}` | First 8 characters of session UUID |

### Path Template Examples

```toml
# Default (sibling directory), used for non-bare repos
path_template = "../{repo-name}-worktrees/{branch}"

# Nested in repo
path_template = "./worktrees/{branch}"

# With session ID for uniqueness
path_template = "../wt/{branch}-{session-id}"
```

## Worktree Warnings

Two classes of non-fatal failures surface through the same warning channel during session create. AOE does not abort the session; instead it captures the failure and surfaces it so you know what to investigate.

| Surface | Where warnings appear |
|---|---|
| CLI (`aoe add`) | `⚠ <message>` line on stderr after `✓ Worktree created successfully` |
| TUI | `Worktree warnings` info dialog opens after the session is added |
| Web | Toast per warning, plus `warnings: string[]` on the `POST /api/sessions` response body |

### Post-checkout hooks

Some repos install pre-commit hooks at the `post-checkout` stage (`uv-sync`, `npm install`, LFS smudge, etc.) that fire when `git worktree add` checks out the new branch. If such a hook fails, the worktree directory and its `.git` pointer have already been created, and the worktree is usable.

Common cause: the hook calls a tool (uv, npm, pip) that needs network access or credentials the new worktree does not yet have. Re-run the hook manually inside the worktree once the environment is set up, or disable it for AOE-created worktrees by configuring `core.hooksPath` per checkout.

### Fetch failures

Before checking out the new branch, AOE runs `git fetch <remote> <branch>` so the worktree starts from the latest remote state. Network errors, missing remotes, SSH key issues, and 10s timeouts no longer pass silently; they surface as warnings shaped like:

```text
git fetch <remote> <branch> failed for <repo>: <stderr>
```

The session is still created when the fetch fails. The worktree branches off whatever local ref already exists, which may be stale. Multi-repo sessions emit one warning per repo whose fetch failed, so a single bad remote in a workspace of five repos shows up as one toast rather than aborting the whole workspace. See issue \#1511 for the rationale.

## Cleanup Behavior

| Scenario | Cleanup Prompt? |
|----------|-----------------|
| aoe-managed worktree | Yes |
| Manual worktree | No |
| `--delete-worktree` flag | Yes (deletes worktree) |
| Non-worktree session | No |

## Bare Repos

AOE auto-detects bare repos and uses `bare_repo_path_template` (default `./{branch}`) instead of `path_template`, creating worktrees as siblings within the project directory. See [Workflow](workflow.md) for the bare-repo setup.

## File Locations

| Item | Path |
|------|------|
| Config | `~/.agent-of-empires/config.toml` |
| Sessions | `~/.agent-of-empires/profiles/<profile>/sessions.json` |

# Worktrees Reference

A managed worktree session gets a branch and checkout, created when the session is created and cleaned up when it is deleted.

## Creating one

```bash
aoe add . -w feat/my-feature -b                          # new branch off the repo default
aoe add . -w hotfix-1 -b --base-branch release-1.2       # new branch off a specific base
aoe add . -w feat/my-feature                             # attach: re-use or check out that branch
aoe worktree list                                        # every worktree
aoe worktree info <session>
aoe worktree cleanup                                     # find orphaned worktrees
aoe remove <session> [--delete-worktree]
```

`-b` is what switches between creating a branch and attaching to an existing one. `--base-branch` only matters with `-b`, and is resolved against the remotes first, then a local branch, so a teammate's not-yet-fetched branch works without a manual `git fetch`. Remote selection scores every configured remote, not just `origin`: in a fork plus `upstream` layout where `upstream/main` is ahead, aoe fetches and branches from there even when you typed `main`. Ties break toward `origin`. Without `--base-branch`, the branch starts from the repo's default.

In the TUI, enable the Worktree checkbox in the new-session dialog (`n`); the directory is derived from the session title. `Ctrl+P` on the Worktree field sets an explicit name, attaches to an existing branch, picks a base branch, or configures extra repos, and `Ctrl+P` on the Base field opens a branch picker over local and remote-tracking branches. The web wizard's **Worktree** switch enables its row of controls, including branch and directory name, a base-branch typeahead, and **Attach to existing branch**. Attaching reuses an existing worktree for that branch, or checks it out into a new worktree.

## Naming

By default, a managed worktree session's title and directory stay tied (`session.tie_workdir_to_name = true`). Scratch, plain tmux and externally attached worktrees are excluded.

- Renaming a session (TUI, web, `aoe session rename`, or `PATCH /api/sessions/{id}`) moves the directory to the title's path-safe slug before committing the title. A failed move leaves the title unchanged.
- The git branch is never renamed by default, since it may carry an upstream or an open PR. Opt in with the TUI rename dialog's "Also rename git branch", `--rename-branch`, or `rename_branch: true`. The TUI warns when the branch tracks a remote, because the remote branch and any open PR do not follow.
- A rename that would relocate the checkout or re-point its branch needs a stopped session and is refused while it runs. A title-only rename whose slug leaves the directory unchanged is allowed on a running session from the CLI and REST (the TUI still asks for a stopped session), and leaves a live structured-view worker alone.

`aoe add -w <branch>` names the session and directory after the branch. Pass `--generated-name` to get a generated name instead (as the TUI and web do without a branch): the title is a civilization name unused by any session, and it fills `{branch}` in the worktree path template, choosing a name whose resulting path is free. The git branch stays exactly as given. A template without `{branch}`, such as `../wt/{session-id}`, never contains the name.

```bash
aoe add . -w feat/my-feature -b --generated-name
```

It conflicts with `--title`. When attaching to a branch that already has a worktree, that directory is kept and only the title is generated. It is refused with `--repo`/`--project`, whose workspace directory comes from `workspace_path_template`. With `session.tie_workdir_to_name` off, the new directory still uses the generated name.

Disable `session.tie_workdir_to_name` in settings or configuration to relabel running sessions freely and edit the directory name independently.

## Editing the Workdir Name After Creation

With `session.tie_workdir_to_name` off, `git worktree move` changes only the final directory component to a path-safe slug. Only AoE-managed worktrees of stopped sessions qualify; refusal changes nothing. The title is unchanged, branch rename is opt-in, and successful changes persist across reload and restart.

| Surface | How |
|---------|-----|
| CLI | `aoe session set-worktree-name <session> --name <new-name> [--rename-branch]` |
| TUI | Select the session and press `W`, or use **Edit worktree workdir name** in the command palette |
| Web | Right-click the row and choose **Edit workdir name** |
| REST | `PATCH /api/sessions/{id}/worktree-name` with `{ "name": "<new-name>", "rename_branch": false }` |

### When the Directory Moves Outside aoe

AoE records a worktree's path and reconciles it against `git worktree list` by branch. The Daemon owns canonical session rows; the TUI displays its snapshots rather than writing the session store. CLI workdir edits can also reconcile paths. Exactly one matching live worktree permits repair; multiple matches leave the recorded path unchanged rather than guessing. A snapshot may briefly retain the old path.

- aoe locks the worktrees it creates, so an out-of-band `git worktree move` needs `git worktree unlock <path>` first.
- A plain `mv` is not recoverable on its own. It leaves git's record naming the old path, which is indistinguishable from a deleted checkout, so aoe reports the worktree as missing and changes nothing. Run `git worktree repair <new-path>` from the repo to bring git's record up to date, and aoe will then find it.

Reconciliation is point-in-time, not a filesystem watcher. A directory moved after reconciliation can stay stale until a later reconciliation or restart.

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

Template variables are `{repo-name}` (repository folder name), `{branch}` (slashes become hyphens), and `{session-id}` (the first 8 characters of the session UUID), so `../wt/{branch}-{session-id}` or `./worktrees/{branch}` both work. Use `[session] row_tag = "branch"` to show branch tags in the TUI list.

### Bare repos

Bare repos are auto-detected and use `bare_repo_path_template` instead, so worktrees land as siblings inside the project directory. A sandboxed session needs this layout to reach the repo's git directory from the container.

### Submodules

After `git worktree add`, a checkout with a `.gitmodules` file gets `git submodule update --init --recursive`. Set `init_submodules = false` (or pass `--no-submodules`) for repos vendoring deep submodule trees, where every new session would otherwise sit in `Creating…` for minutes. On delete, AoE runs `git worktree remove`. If Git refuses because of submodules, AoE removes `<main>/.git/worktrees/<name>/modules/` and the checkout, then prunes the stale entry.

## Cleanup

Deleting a session prompts to remove an AoE-managed worktree, or pass
`--delete-worktree`. Manual worktrees and non-worktree sessions are left alone.

### Trashing relocates the worktree

Trashing a managed worktree session moves its checkout into a sibling `.aoe-trash/<session-id>` holding directory via `git worktree move`. If relocation fails, the session remains in the trash and its recorded path is unchanged. The default branch checkout is protected as described below.

Restore moves the worktree back to its original path. If that path is occupied, the session stays in the trash and nothing is overwritten; clear the conflict and retry. Restoring leaves the agent stopped. Start it explicitly when ready to resume. Purging a trashed session removes the worktree from the holding directory. The Daemon reconciles interrupted relocations and publishes the resulting rows to the TUI.

Without Force, purge preserves a dirty worktree and retains its session for retry. A workspace purge also stops when a sibling was restored, preserving the owner and its shared worktree.

Native daemon clients can abandon a stuck purge using its current reservation generation. Abandon removes the session record and schedules runtime cleanup; it does not cancel a hook or cleanup already underway, and does not start another worktree or branch removal.

**The default branch's checkout is never removed.** A bare-repo layout keeps
that checkout available to other tooling. AoE refuses to remove it or delete
its branch, reports the refusal, and deletes the session record instead.
Force does not bypass this protection. Detection uses the bare repository's
`HEAD` and every remote's `refs/remotes/<remote>/HEAD`, falling back to `main`
and `master` only when neither identifies a default. Use git explicitly if
you intend to remove a protected checkout.

An external `git worktree lock` is not a deletion guard: AoE unlocks worktrees before intentional removal or moving, including external locks. AoE attempts to lock new worktrees; a lock failure warns without aborting creation.

## Warnings during create

Two kinds of non-fatal failure surface through the same channel instead of aborting the session: a `⚠` line on stderr for `aoe add`, a **Worktree warnings** dialog in the TUI, and a toast plus `warnings: string[]` on the `POST /api/sessions` response for the web.

### Post-checkout hooks

Some repos install pre-commit hooks at the `post-checkout` stage (`uv-sync`, `npm install`, LFS smudge) that fire when `git worktree add` checks out the branch. If one fails, the worktree and its `.git` pointer already exist and are usable. Usually the hook needs network access or credentials the new worktree does not have yet: re-run it by hand once the environment is set up, or set `core.hooksPath` per checkout.

### Fetch failures

AoE fetches the selected branch before checkout. Network errors, missing
remotes, SSH failures and timeouts surface as `git fetch <remote> <branch>
failed for <repo>: <stderr>`. If a usable ref remains, creation can continue
from it, although it may be stale. An explicit base with no usable ref fails
with `BranchNotFound`. Multi-repo workspaces report each failed repo.

## File Locations

Configuration and session data live under the app directory; see [File Locations](configuration.md#file-locations).

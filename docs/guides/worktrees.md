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

```toml
[session]
tie_workdir_to_name = true
```

When tied:

- Renaming a session (TUI rename, web inline rename, `aoe session rename`, or `PATCH /api/sessions/{id}`) moves the worktree directory to the title's path-safe slug before committing the title. A failed move leaves the title unchanged. A metadata failure after a successful move is reported as a partial failure.
- The git branch is never swept in by a title rename by default. To rename it too, check "Also rename git branch" in the TUI rename dialog, pass `--rename-branch` to `aoe session rename`, or send `rename_branch: true` to the PATCH. It stays opt-in because a branch may carry an upstream or an open PR; the TUI toggle warns when the branch tracks a remote, since the remote branch (and any open PR) won't follow the local rename.
- A tied rename that relocates the worktree directory, or renames its branch, needs a stopped session: moving or re-pointing a live checkout is unsafe, so it is refused with a clear message while the session is active. Stop the session, or disable the setting, to relabel it freely.
- A title-only rename whose slug leaves the directory unchanged moves no checkout. `PATCH /api/sessions/{id}` and `aoe session rename` accept it while the session runs unless `rename_branch: true` would change the branch. The PATCH also leaves a live structured-view worker alone. The TUI requires a stopped session for a title-changing tied rename.
- Naming collapses into the single rename action: the standalone "edit workdir name" affordance is hidden (TUI and web) and the standalone CLI / REST workdir-name edit is rejected, since the directory now follows the title.

`aoe add -w <branch>` names the session and directory after the branch. Pass `--generated-name` to get a generated name instead (as the TUI and web do without a branch): the title is a civilization name unused by any session, and it fills `{branch}` in the worktree path template, choosing a name whose resulting path is free. The git branch stays exactly as given. A template without `{branch}`, such as `../wt/{session-id}`, never contains the name.

```bash
aoe add . -w feat/my-feature -b --generated-name
```

It conflicts with `--title`. When attaching to a branch that already has a worktree, that directory is kept and only the title is generated. It is refused with `--repo`/`--project`, whose workspace directory comes from `workspace_path_template`. With `session.tie_workdir_to_name` off, the new directory still uses the generated name.

Toggle the setting off (TUI settings, web settings, or the toml above) to relabel sessions freely while running and to edit the directory name independently of the title.

## Editing the Workdir Name After Creation

With `session.tie_workdir_to_name` off, `git worktree move` changes only the final directory component to a path-safe slug. Only AoE-managed worktrees of stopped sessions qualify; refusal changes nothing. The title is unchanged, branch rename is opt-in, and successful changes persist across reload and restart.

| Surface | How |
|---|---|
| CLI | `aoe session set-worktree-name <session> --name <new-name> [--rename-branch]` |
| TUI | Select the session and press `W`, or use **Edit worktree workdir name** in the command palette |
| Web | Right-click the row and choose **Edit workdir name** |
| REST | `PATCH /api/sessions/{id}/worktree-name` with `{ "name": "<new-name>", "rename_branch": false }` |

### When the Directory Moves Outside aoe

AoE records a worktree's path and reconciles it against `git worktree list` by branch. The Daemon owns canonical session rows; the TUI displays its snapshots rather than writing the session store. CLI workdir edits can also reconcile paths. Exactly one matching live worktree permits repair; multiple matches leave the recorded path unchanged rather than guessing. A snapshot may briefly retain the old path.

Two caveats:

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

Use `[session] row_tag = "branch"` to show worktree and workspace branch tags in
the TUI session list, or `"none"` to hide suffix metadata next to session names.

### Skipping submodule init

`init_submodules = false` skips the `git submodule update --init --recursive` step that runs after `git worktree add` when the checkout contains a `.gitmodules` file. Useful for repos that vendor deep submodule trees (e.g. OpenROAD-flow-scripts, llvm-project, chromium) where every new session would otherwise sit in `Creating…` for minutes while submodules clone. Per-invocation override on the CLI: `aoe add --worktree <branch> --no-submodules`.

On the delete side, aoe runs `git submodule deinit -f --all` before `git worktree remove` for any worktree with `.gitmodules`, so the panic-button `Force` checkbox is not required just because the worktree has submodules. If git still refuses (e.g. a partially-broken submodule), aoe falls back to clearing `<main>/.git/worktrees/<name>/modules/` and pruning the stale entry manually.

### Trashing relocates the worktree

Trashing a managed worktree session moves its checkout into a sibling `.aoe-trash/<session-id>` holding directory via `git worktree move`. If relocation fails, the session remains in the trash and its recorded path is unchanged. The default branch checkout is protected as described below.

Restore moves the worktree back to its original path. If that path is occupied, the session stays in the trash and nothing is overwritten; clear the conflict and retry. Restoring leaves the agent stopped. Start it explicitly when ready to resume. Purging a trashed session removes the worktree from the holding directory. The Daemon reconciles interrupted relocations and publishes the resulting rows to the TUI.

Without Force, purge preserves a dirty worktree and retains its session for retry. A workspace purge also stops when a sibling was restored, preserving the owner and its shared worktree.

Native daemon clients can abandon a stuck purge using its current reservation generation. Abandon removes the session record and schedules runtime cleanup; it does not cancel a hook or cleanup already underway, and does not start another worktree or branch removal.

### The default branch's checkout is never removed

A bare-repo layout (git dir at `<project>/.bare`, default branch checked out at `<project>/main`) keeps the default branch in a linked worktree that other tooling expects to stay put. AOE therefore refuses to remove that checkout, refuses to delete the branch, and leaves the checkout where it is when the session is trashed. The refusal is reported as a message and the session is still deleted; only the worktree and branch survive.

Detection uses what git itself states: a bare repo's own `HEAD`, plus every remote's `refs/remotes/<remote>/HEAD`. When neither exists (a repo that never ran `git remote set-head`), local `main` and `master` are protected by convention. A repo that explicitly names some other default leaves a branch merely called `main` deletable.

Force does not bypass this, including the forced trash auto-purge and `aoe session empty-trash`. `aoe worktree cleanup` lists such a checkout as skipped rather than removing it. To remove one anyway, do it with git (`git worktree remove`, `git branch -D`) and then delete the session.

Note that an externally placed `git worktree lock` is not a deletion guard: AOE locks every worktree it creates and unlocks before every intentional remove or move, so it unlocks yours too.

### Template Variables

`{repo-name}` is the repository folder name, `{branch}` replaces branch slashes with hyphens, and `{session-id}` is the first eight UUID characters. For example, use `../{repo-name}-worktrees/{branch}`, `./worktrees/{branch}`, or `../wt/{branch}-{session-id}`.

## Worktree Warnings

Non-fatal creation failures surface as a `⚠` line on CLI stderr, a **Worktree warnings** TUI dialog, or a web toast with `warnings: string[]` in the `POST /api/sessions` response. The session is still created.

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

Deleting a session prompts for cleanup of AoE-managed worktrees; pass `--delete-worktree` to request removal. Manual worktrees and non-worktree sessions are left alone. See the [CLI Reference](../cli/reference.md) for commands.

## Bare Repos

AOE auto-detects bare repos and uses `bare_repo_path_template` (default `./{branch}`) instead of `path_template`, creating worktrees as siblings within the project directory.

## File Locations

Configuration and session data live under the app directory; see [File Locations](configuration.md#file-locations).

# Scratch sessions

A scratch session is a session that does not belong to any project on
disk. When you start one, AoE provisions a fresh directory under
`~/.agent-of-empires/scratch/<id>/` (on Linux:
`$XDG_CONFIG_HOME/agent-of-empires/scratch/<id>/`), attaches the
session to it, and removes the directory when you delete the session
(unless you opt in to keeping it).

Use scratch sessions for one-off questions, quick investigations, or
ad-hoc agent runs where you do not want a session record tied to a
specific repo or to keep stray files around afterwards.

## When to use it

* You want to ask the agent a question that does not depend on a
  particular codebase.
* You want a clean, empty scratch directory for the agent to write
  files into without polluting an existing project.
* You want to investigate something quickly and have the directory
  go away automatically when you are done.

## Three ways to start one

### Command line

```bash
aoe add --scratch -t "Quick question" -c claude
```

The session prints its resolved `Path:` line pointing at
`~/.agent-of-empires/scratch/<id>/` and a `Scratch: yes` line in the
summary. You do not pass a project path; it is provisioned for you.

Trying to pass a path alongside `--scratch` is rejected:

```bash
aoe add /Users/me/repo --scratch
# error: Cannot specify a project path with --scratch
```

`--scratch` is mutually exclusive with all worktree-related flags
(`-w`, `--new-branch`, `--base-branch`, `--repo`, `--project`,
`--no-submodules`). Mixing them fails at parse time with a clear
conflict message.

### Web dashboard

In the new-session wizard, the **Project** step has a toggle labeled
**Skip project folder** above the Recent / Browse / Clone URL tabs.
Turning it on hides the path picker and the worktree controls on the
Session step. The Review step shows
"Scratch directory (provisioned on create)" in place of the path.

Selecting a real project (Recent / Browse / Clone) turns the toggle
back off, so the wizard never submits a request with both a real path
and the scratch flag set.

**Fast-create shortcut.** From anywhere in the dashboard, press
`Cmd+Shift+N` (Mac) or `Ctrl+Shift+N` (Linux / Windows) to open the
wizard with scratch already enabled and jumped to the Review step.
A follow-up `Cmd+Enter` / `Ctrl+Enter` launches the session, so two
keystrokes is enough to spin up a fresh scratch session.

**Command palette.** The same flow is reachable from the command
palette (`Cmd+K` / `Ctrl+K`): search for "New scratch session" and
run it to open the wizard prefilled for a scratch session. In
read-only mode the creation commands are hidden from the palette.

**Sidebar grouping.** Every scratch session has its own
`scratch/<id>/` directory on disk, so the dashboard sidebar would
otherwise render each one as its own one-session group. Scratch
sessions are bucketed into a single synthetic **Scratch** group at
the bottom of the sidebar instead, mirroring how multi-repo
workspaces are collapsed into one **Multi-repo** group. Clicking the
"+" on the Scratch group header opens the wizard with no path
prefilled, so you can flip on **Skip project folder** to add another
one.

### TUI new-session dialog

Press `Ctrl+T` from any field in the new-session dialog. The Path
input is replaced with a `(scratch directory)` marker, the worktree
toggle is forced off, and submitting creates the session in a fresh
scratch directory. The bottom hint line surfaces `Ctrl+T scratch` so
the binding is always visible; the chip is emphasized when you are
focused on the Path row. Press `Ctrl+T` again to revert.

## Storage location

Scratch directories live under the app data dir, not the system temp
directory. The exact location depends on platform:

| Platform | Scratch root |
| --- | --- |
| macOS / Windows | `~/.agent-of-empires/scratch/` |
| Linux | `$XDG_CONFIG_HOME/agent-of-empires/scratch/` (defaults to `~/.config/agent-of-empires/scratch/`) |

Each session gets its own `<instance-id>/` subdirectory. The
basename is the session's stable instance id so it is easy to map
back when you peek at the folder.

Storing under the app dir (rather than `$TMPDIR`) means scratch
directories survive OS-level temp-dir cleaning (`systemd-tmpfiles` on
Linux, macOS reboot cleanup) until you explicitly delete the session.

## What happens at delete

When you delete a scratch session via `aoe rm`, the web dashboard
delete flow, or the TUI delete dialog, the deletion path also runs
`fs::remove_dir_all` on the session's scratch directory. The cleanup
is guarded: it only runs when the session's `scratch` flag is true
AND the path lives under the scratch root. A session record with a
tampered `project_path` pointing at, say, `/etc` is left alone.

Pass `--keep-scratch` to `aoe rm` (or check the corresponding box in
the delete dialog) to keep the directory on disk. The session is
detached from AoE's view but the files survive. The kept path is
logged so you can find it later.

The wizard's **Recent projects** tab filters scratch sessions out
once they exist, so a deleted scratch directory does not appear as a
reusable recent project. Multi-repo workspace sessions are filtered out
of the same list, since the project step cannot rebuild a workspace from
a single path; picking one would start a plain single-repo session and
silently drop the other repos.

## Compatibility

* **Structured view**: scratch sessions use the structured view (the default) with the
  bundled ACP agents. The ACP worker spawns with the scratch
  directory as its current working directory.
* **Sandboxes**: scratch sessions can run in a container sandbox
  (`-s` or `--sandbox-image`); the container mounts the scratch
  directory the same way it mounts a real project path.
* **Worktrees**: not supported. A scratch directory is not a git
  repo, so the worktree concept does not apply. Use a regular project
  path with `-w` if you want a worktree.
* **Hooks**: a scratch directory has no `.agent-of-empires/config.toml`,
  so the per-repo hook trust prompt never fires. Global and profile
  `on_create` hooks still run, with the scratch directory as their
  `cwd`.

## Cleanup and retention

If `aoe serve` (or your shell session) dies before you delete a
scratch session, the directory is left on disk. There is no automatic
retention policy yet; you can clean up manually by deleting the
session record (which removes the directory) or by removing entries
under the scratch root for your platform (see the table above)
directly. A daemon-side orphan sweep on `aoe serve` startup is
tracked as a follow-up.

If you need the session to outlive its scratch directory (rename,
move, keep the files), use `aoe rm --keep-scratch` and copy the files
out, or create a new session against a real path with
`aoe add <path>`.

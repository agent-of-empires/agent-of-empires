# Scratch sessions

A scratch session belongs to no project on disk. AoE provisions a fresh directory under `~/.agent-of-empires/scratch/<id>/` (Linux: `$XDG_CONFIG_HOME/agent-of-empires/scratch/<id>/`), attaches the session to it, and removes it when you delete the session. Use one for a question that does not depend on a codebase, or to give an agent an empty directory to write into.

Storing them under the app dir rather than `$TMPDIR` means they survive OS-level temp cleaning until you delete the session.

## Starting one

```bash
aoe add --scratch -t "Quick question" -c claude
```

You pass no project path; it is provisioned for you, and the summary prints the resolved `Path:` and `Scratch: yes`. Passing a path alongside `--scratch` is rejected, as is combining it with any worktree flag (`-w`, `--new-branch`, `--base-branch`, `--repo`, `--project`, `--no-submodules`), which fails at parse time.

**Web**: the wizard's Project step has a **Skip project folder** toggle above the Recent / Browse / Clone tabs; picking a real project turns it back off, so the wizard never submits both. `Cmd/Ctrl+Shift+N` opens the wizard with scratch already on, and `Cmd/Ctrl+Enter` launches, so two keystrokes is enough. The command palette has "New scratch session" too. Scratch sessions are bucketed into one synthetic **Scratch** group at the bottom of the sidebar rather than one group per directory.

**TUI**: press `Ctrl+T` from any field in the new-session dialog. The Path input becomes a `(scratch directory)` marker and the worktree toggle is forced off; `Ctrl+T` again reverts.

## Deleting one

Deleting the session (`aoe rm`, the web dashboard, or the TUI delete dialog) removes its directory. Pass `--keep-scratch` (or tick the box in the delete dialog) to keep the files; the path is logged and the session is detached from AoE's view. Kept directories do not appear in the wizard's Recent tab.

If a process dies before you delete the session, the directory is left on disk. There is no retention policy yet: delete the session record, or remove entries under the scratch root yourself.

## Compatibility

- **Structured view**: supported, with the scratch directory as the worker's working directory.
- **Sandboxes**: supported; the container mounts the scratch directory like any project path.
- **Worktrees**: not supported, since a scratch directory is not a git repo. Use a real project path with `-w`.
- **Hooks**: a scratch directory has no `.agent-of-empires/config.toml`, so the repo trust prompt never fires. Global and profile `on_create` hooks still run, with the scratch directory as their `cwd`.

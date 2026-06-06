# Workflow Guide

This guide covers the recommended setup and daily workflow for using `aoe` with git worktrees.

## Project Setup: Bare Git Repos

The recommended setup is a "bare repo" structure, keeping the main repository and all worktrees under one directory:

```
my-project/
  .bare/               # Bare git repository
  .git                 # File pointing to .bare
  main/                # Worktree for main branch
  feat-api/            # Worktree for feature branch
  fix-bug/             # Another worktree
```

### Initial Setup

```bash
git clone --bare git@github.com:user/repo.git my-project/.bare
cd my-project
echo "gitdir: ./.bare" > .git
git config remote.origin.fetch "+refs/heads/*:refs/remotes/origin/*"
git fetch origin
git worktree add main main
```

Run `aoe` from `my-project/` and new worktrees are created as siblings (e.g. `my-project/feat-api/`) rather than in a separate directory.

Bare repos keep all paths within the project root (required for Docker sandboxing) and let you switch branches by switching directories.

## Single-Window Workflow

Run `aoe` in a single terminal and toggle between views:

| Key | View | Purpose |
|-----|------|---------|
| (default) | Structured View | Manage and interact with AI coding agents |
| `t` | Terminal View | Access paired terminals for git, builds, tests |

### Daily Workflow

**1. Start your day**

```bash
cd ~/scm/my-project
aoe
```

You'll see your sessions in Structured View. Keep one session on `main` for general questions and pulling updates.

**2. Update main** (Terminal View)

- Press `t` to switch to Terminal View
- Select your main session, press `Enter` to attach to its terminal
- Run `git pull origin main`
- Detach with `Ctrl+b d`
- Press `t` to return to Structured View

**3. Create a new session**

- Press `n` to open the new session dialog
- Enter a session title, for example `Auth Refactor`
- Enable Worktree if it is not already checked
- Press `Enter`

This creates:
- A new branch derived from the title, for example `auth-refactor`
- A new worktree using that branch name
- A new session with an agent working in that worktree

To override the generated name, focus Worktree and press `Ctrl+P`, then fill in `Name`.

**4. Work on your feature** (Structured View)

- Select your session and press `Enter` to attach
- Interact with the agent
- Detach with `Ctrl+b d` when done

**5. Run builds/tests** (Terminal View)

- Press `t` to switch to Terminal View
- Select the same session, press `Enter`
- Run your build commands, tests, git operations
- Detach with `Ctrl+b d`

**6. Clean up when done**

- In Structured View, select the session and press `d` to delete
- Answer `Y` to also remove the worktree

## Tips

- **Keep one session on main**: Use it for codebase questions and its terminal for `git pull`
- **One task, one session**: Each worktree maps to one aoe session. Keeps context isolated.
- **Pull before creating**: Always update main before creating new sessions so branches start fresh
- **Let agents stay focused**: Git operations happen in the paired terminal, not in agent sessions

## Keyboard Reference

| Key | Action |
|-----|--------|
| `t` | Toggle between Structured View and Terminal View |
| `D` | Open [Diff View](diff-view.md) to review git changes |
| `Enter` | Attach to agent (Structured View) or terminal (Terminal View) |
| `n` | Create new session |
| `d` | Delete session (Structured View only) |
| `?` | Show help |
| `Ctrl+b d` | Detach from tmux (return to aoe) |

## Non-Bare Repos

If you're not using a bare repo setup, aoe defaults to creating worktrees in a sibling directory:

```
~/scm/
  my-project/              # Your repo (stays on main)
  my-project-worktrees/    # Worktrees created here
    feat-auth-refactor/
    fix-bug/
```

You can customize this with `path_template` in your config. See the [Worktrees Reference](worktrees.md) for details.

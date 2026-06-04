# Diff View

The diff view lets you review changes between your working directory and a base branch (like `main`), then edit files directly.

## Opening Diff View

From the main screen, press `D` to open the diff view. It shows:
- **Left panel**: List of changed files with status indicators (M=modified, A=added, D=deleted)
- **Right panel**: Diff content for the selected file

The diff is computed against the base branch (defaults to `main` or your repo's default branch).

Auto-detection scores every configured remote, not just `origin`. In a fork plus `upstream` layout, the diff compares against `upstream/main` when that tip is fresher than your local `main` and `origin/main`. The chosen ref is whichever candidate HEAD descends from with the most recent commit time, so a worktree branched off `upstream/main` does not see the gap between your stale fork-main and the actual branch point as session changes.

## Navigation

| Key | Action |
|-----|--------|
| `j` / `k` or `↑` / `↓` | Navigate between files |
| Scroll wheel | Scroll through diff content |
| `PgUp` / `PgDn` | Page through diff |
| `g` / `G` | Jump to top / bottom of diff |

## Split view

Diffs can be shown in a **split** layout (side-by-side: old on the left, new on the right) in addition to the default unified layout. Pure additions and deletions appear on their own side with an aligned placeholder opposite them; context lines show on both sides.

- **TUI**: press `s` to toggle split vs unified. The choice is saved to `[diff].split_view` and restored on the next launch (also editable in the settings TUI under **Diff**). On a narrow diff pane the view falls back to unified automatically.
- **Web dashboard**: use the **Unified/Split** toggle in the diff header, or **Settings → Diff**. The preference is stored per browser and the view falls back to unified on narrow screens. Inline comments work in either layout.

## Editing Files

Press `e` or `Enter` to open the selected file in your editor (`$EDITOR`, or vim/nano if not set).

After saving and exiting, the diff view refreshes automatically to show your changes.

## Other Commands

| Key | Action |
|-----|--------|
| `s` | Toggle split/unified layout |
| `b` | Change base branch (persists per-session as `base_branch_override`) |
| `r` | Refresh the diff |
| `y` | Copy the selected file's relative path to the clipboard |
| `?` | Show help |
| `Esc` | Close diff view |

## Copying a file's path

Copy a changed file's repo-relative path to the clipboard:

- **TUI**: press `y` (yank) on the selected file. A `Copied <path>` confirmation shows in the footer.
- **Web dashboard**: right-click a file in the Changes list (or a folder row in tree view) and choose **Copy relative path**. A `Copied <path>` toast confirms it.

The path is relative to the file's repository root, so it pastes straight into commands or comments.

## Commenting on the diff (web only, structured view sessions)

The web dashboard lets you annotate lines in the diff and send the
comments to the agent as a single prompt (#928).

- Hover any line in the diff viewer to reveal a `+` button in the
  left gutter. Click it to start a comment.
- Click the `+` on the same line again to comment on that single
  line, or click `+` on another line in the **same hunk** to extend
  the range. Cross-hunk ranges are not allowed.
- An inline form opens beneath the last line of the range. The body
  supports markdown; `Cmd/Ctrl+Enter` saves, `Esc` cancels.
- Saved comments render inline as cards with edit / delete actions
  and a markdown-rendered body. Each card shows the line range it's
  anchored to.
- The right panel surfaces a banner above the diff file list once
  you have at least one comment, with a Send button (or
  `Cmd/Ctrl+Shift+S`).
- The send dialog has three pieces: an editable intro textarea, a
  read-only markdown preview of the assembled comments (each with
  its captured code snippet so the agent can act without re-reading
  the file), and an editable outro textarea (default "Please
  address these comments."). Send fires `POST
  /api/sessions/:id/acp/prompt`. Comments are cleared on
  success unless you uncheck "Clear comments after sending".
- Comments persist in `localStorage`, scoped per session. They
  survive page reloads but are browser-local.
- If a line range becomes unreachable in the current diff (agent
  edited the file), the comment moves to a "stale comments" block
  at the top of the file view with a `[stale]` chip; the captured
  snippet still goes through to the agent in the prompt.
- The feature is hidden for non-structured view sessions (tmux/PTY). The
  Send button is also disabled while the structured view worker isn't
  running.

## Per-session base override

Each session has an optional `base_branch_override` that takes
precedence over the profile default and auto-detection. Use it when
the eventual PR target differs from the project default (stacked PRs,
hotfix off `release/*`, branch rename). The override is sticky across
restarts and only affects the comparison, not the worktree itself
(no rebase). See #970.

- **Web dashboard**: click the `vs <ref>` chip in the diff header, pick
  a branch from the typeahead (local + remote-only), or use
  "Reset to auto-detected" to clear.
- **TUI diff view**: press `b`, pick a branch; the choice is persisted
  to `sessions.json` and restored on next launch.
- **CLI**: `aoe session set-base <session> <branch>` to set,
  `aoe session set-base <session> --clear` to clear.

## Configuration

In your config file (`~/.config/agent-of-empires/config.toml` on Linux, `~/.agent-of-empires/config.toml` on macOS):

```toml
[diff]
# Default branch to compare against (auto-detected if not set)
default_branch = "main"

# Lines of context around changes (default: 3)
context_lines = 3

# Show diffs in a split layout instead of unified (default: false)
split_view = false
```

## Tips: See Changes While Editing

The diff view shows you where changes are before you edit. For an even better experience, you can install editor plugins that show git diff markers in the gutter while you edit:

### Vim

Install [vim-gitgutter](https://github.com/airblade/vim-gitgutter) or [vim-signify](https://github.com/mhinz/vim-signify). These show `+`, `-`, and `~` markers in the sign column for added, removed, and modified lines.

With vim-plug:
```vim
Plug 'airblade/vim-gitgutter'
```

### Nano

Nano doesn't have a plugin system, so there's no equivalent. Use the diff view to note line numbers before editing, or consider switching to vim for this workflow.

### Other Editors

- **Emacs**: [git-gutter](https://github.com/emacsorphanage/git-gutter)
- **VS Code**: Built-in git gutter support
- **Sublime Text**: [GitGutter](https://packagecontrol.io/packages/GitGutter)

## Workflow Example

1. Press `D` to open diff view
2. Use `j`/`k` to browse changed files
3. Scroll to review each file's changes
4. Press `e` to edit a file that needs work
5. Save and exit the editor
6. Continue reviewing (diff auto-refreshes)
7. Press `Esc` when done

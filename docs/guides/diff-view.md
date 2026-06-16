# Diff View

The diff view lets you review changes between your working directory and a base branch (like `main`), then edit files directly.

## Opening Diff View

From the main screen, press `D` to open the diff view. It shows:
- **Left panel**: List of changed files with status indicators (M=modified, A=added, D=deleted)
- **Right panel**: Diff content for the selected file

The diff is computed against the base branch (defaults to `main` or your repo's default branch). Auto-detection considers every configured remote, not just `origin`, so a fork plus `upstream` layout compares against the branch point rather than a stale fork-main. When the base resolves to a local branch that is strictly behind its `origin/` tracking counterpart (your local `main` drifted behind `origin/main`), the diff compares against the remote tip, so upstream commits you haven't pulled don't show up as session changes.

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

The web dashboard lets you annotate diff lines and send the comments to the
agent as a single prompt.

1. Hover a line and click the `+` in the left gutter to start a comment. Click
   `+` on another line in the **same hunk** to extend the range (cross-hunk
   ranges are not allowed).
2. Write the comment in the inline form (markdown supported). `Cmd/Ctrl+Enter`
   saves, `Esc` cancels. Saved comments render inline as cards with edit/delete.
3. Once you have a comment, a banner appears above the file list with a **Send**
   button (or `Cmd/Ctrl+Shift+S`). The send dialog has an editable intro, a
   preview of the assembled comments (each with its captured snippet), and an
   editable outro. Comments clear on success unless you uncheck "Clear comments
   after sending".

Comments persist in `localStorage` per session (browser-local). If the agent
edits a file so a range no longer matches, the comment moves to a "stale
comments" block with a `[stale]` chip; the captured snippet still goes to the
agent. The feature is hidden for non-structured view sessions, and Send is
disabled while the worker isn't running.

## Per-session base override

Each session can override the branch it diffs against. Use it when the eventual
PR target differs from the project default (stacked PRs, hotfix off `release/*`,
branch rename). The override is sticky across restarts and only changes the
comparison, not the worktree (no rebase).

Comparison precedence: per-session override, then the branch the worktree was
forked from, then `diff.default_branch`, then auto-detection.

- **Web dashboard**: click the `vs <ref>` chip in the diff header, pick a branch
  from the typeahead, or reset to clear the override.
- **TUI diff view**: press `b`, pick a branch.
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

To show git diff markers in your editor's gutter while editing:

- **Vim**: [vim-gitgutter](https://github.com/airblade/vim-gitgutter) or [vim-signify](https://github.com/mhinz/vim-signify) (`Plug 'airblade/vim-gitgutter'`).
- **Emacs**: [git-gutter](https://github.com/emacsorphanage/git-gutter).
- **VS Code**: built-in.
- **Sublime Text**: [GitGutter](https://packagecontrol.io/packages/GitGutter).
- **Nano**: no plugin system; note line numbers from the diff view before editing.

## Workflow Example

1. Press `D` to open diff view
2. Use `j`/`k` to browse changed files
3. Scroll to review each file's changes
4. Press `e` to edit a file that needs work
5. Save and exit the editor
6. Continue reviewing (diff auto-refreshes)
7. Press `Esc` when done

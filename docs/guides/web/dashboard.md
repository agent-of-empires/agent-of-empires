# Dashboard & Workspaces

The dashboard is the home screen of the web app: a workspace sidebar on the left, the active session in the main pane, and a top bar with global actions. This page covers the layout, creating a session, and keeping a long session list under control. For running the server and auth, see the [Web Dashboard overview](../web-dashboard.md).

![The dashboard with the workspace sidebar, session summary, and status glyphs](../../assets/web/dashboard.png)

## Layout

- **Workspace sidebar** (left) lists every session grouped by repo, with a live status glyph per row. On phones it collapses behind a top-bar toggle. With no sessions, it shows a hint and a **New session** button.
- **Main pane** shows the selected session: the agent terminal (or structured view), with the diff and paired terminal reachable from the top bar.
- **Top bar** carries the command-palette trigger, the right-panel picker, and the overflow (three-dot) **More options** menu.
- **Home screen** (no session selected) shows the AoE logo and a count of running, waiting, and error sessions.

### Status glyphs

Each sidebar row carries an animated braille glyph encoding the session's state: a spinner of dots while **Running**, an orbiting dot while **Waiting** or **Creating**, a slow breathe while **Starting** or freshly idle. Errors render in the error color. The frame is offset by each session's creation time so rows don't pulse in lockstep.

## Creating a session

The **New session** wizard walks four steps:

- **Project**: pick the working directory from the Recent tab, browse for one, clone a URL, or start a scratch session with no path. The Recent tab lists your saved projects under a "Saved projects" section above the directories of your recent sessions. The recent list keeps a project around after its last session is deleted, so you can quickly start there again; entries whose directory no longer exists are dropped.
- **Session**: set the title (auto-slugifies into a worktree branch name unless you edit the branch), or attach an existing branch instead.
- **Agent**: select the tool and profile, plus per-session knobs (auto-approve / YOLO mode, "Run in a safe container" sandbox, command override, extra args / env).
- **Review**: confirm before the session spawns.

Choosing a profile seeds the agent-step defaults. If you have already edited a field, switching profiles asks before overwriting it.

## Command palette

The command palette (top-bar button or keyboard shortcut) is a fuzzy launcher for global actions: jump to a session, open settings, start a new session, toggle the right panel.

Individual settings also appear in the palette under `Settings`. A writable toggle flips inline from the palette (a toast confirms, and the subtitle shows its current state and scope); every other setting opens the settings view on its tab. Read-only servers and settings that need elevation jump to the settings view instead of writing. The Settings header also has its own search box (and the TUI settings screen the `/` key) that filters settings across every tab and jumps to the one you pick.

## First-run onboarding

The first time you open the dashboard in a browser, a **Choose your theme** card appears before anything else. Picking a theme applies it live and saves it to your default profile; you can switch freely, then click **Continue**. Change it later in Settings > Appearance. The card is skipped in read-only mode and for anyone who already finished the tutorial.

After the theme card, an interactive walkthrough highlights the major regions (command bar, sidebar, starting a session, settings, and inside a session the diff panel and composer). Two of its steps open Settings for you: the Worktree tab, explaining the per-session path templates, and the Plugins tab, explaining how to find, install, and trust plugins. Each step lists its keyboard shortcuts and has a **Skip** button.

Completing or skipping the tour records `app_state.has_seen_web_tour` on the server, so it does not relaunch on reload or on another device pointed at the same server. That flag persists in the server's `state.toml` (a sibling of `config.toml`, see [Configuration Reference](../configuration.md#statetoml)); the `GET /api/settings` JSON key is unchanged, still `app_state.has_seen_web_tour`. To replay it, open the overflow menu and choose **Show tutorial**; re-triggering adapts to where you are (dashboard regions, or composer / mode picker / send controls inside a session). It does not auto-launch on touch devices, where it is menu-only.

## Sidebar sort

By default the sidebar shows your manually-ordered list. Drag a row (press-and-hold) to move it; the order persists across browsers and devices via `workspace-ordering.json`. To reorder whole projects, press-and-drag the project/group header itself (there is no separate handle); this group order is per-browser (localStorage), not synced. Group drag is disabled while a filter is active or a computed sort mode is selected.

Each project header shows the project icon next to its name, with a count of the sessions it holds. Hovering the header swaps the icon for a fold chevron; clicking the header collapses or expands the project's sessions.

A sort picker next to the filter button offers three modes:

- **Manual** (default): keeps your drag-ordered list, drag enabled.
- **Recent activity**: orders by the most recent of `last_accessed_at`, `idle_entered_at`, and `created_at` across each workspace's sessions, descending.
- **Attention**: floats sessions needing a human to the top, mirroring the TUI's Attention sort. Ranks by status (Waiting, Error, Idle, Unknown, Running, Stopped, transient states last); sessions flagged urgent via the `attention-urgent` hook rise above non-urgent rows in their tier, favorited rows come first within a status rank, ties break by most-recent activity.

Drag-to-reorder is disabled in the computed modes. The picker's state is per-browser (localStorage), not synced and not tied to your profile. The Multi-repo and Scratch groups default to the bottom; in manual mode you can drag them anywhere, in computed modes they stay at the bottom.

## Sidebar grouping: by repo, by group, or both

A grouping toggle (layers icon) next to the sort toggle cycles the axis. Each click advances **By repo** to **By group** to **By repo and group** and back:

- **By repo** (default): groups by git repository.
- **By group**: groups by the user-defined group assigned in the TUI rename dialog, with `aoe group move`, or via **Edit group** below. Ungrouped sessions fall into an **Ungrouped** bucket pinned to the bottom.
- **By repo and group**: repository headers with user groups nested inside each. A session split across groups appears once per subgroup.

The choice is per-browser (localStorage). Collapse state is tracked separately per axis. You can move a session between groups from the web context menu, but group rename, color, and drag-reorder live on the repo axis only.

**Edit group.** Right-click (long-press on touch) a session row and choose **Edit group**: type an existing group to move it, a new path to create that group, or clear the field to drop it back to **Ungrouped**. Group paths use `/` for hierarchy (e.g. `work/projects`). Hidden in read-only mode.

## Triage: pin, archive, snooze

The right-click (long-press on touch) context menu on any session row exposes three triage primitives:

- **Pin**: floats the workspace to the top in every sort mode. Pin is web-only and distinct from the favorite mark. On the web, favorite stays a within-tier Attention signal; in the TUI it pins to the top of its sibling scope in every sort order while `session.favorites_first` is on (the default). Renders as a pushpin glyph.
- **Archive**: tears down every tmux session the workspace owns (agent, web terminal, container terminal, and tool sub-sessions; pass `kill_pane: false` in the API body, or `--no-kill` on the CLI, to skip the tmux teardown) and shuts down the structured-view worker for ACP sessions, then sinks the row into the collapsible "Snoozed & archived" footer. Sending a message wakes it back into the live list. Daemon restarts skip archived sessions. See #1868.
- **Snooze**: sinks the row for a chosen duration (presets: 1h, 2h, 3h, 4h, 5h, 6h, 1d, 1w). Wakes when the timer expires; sending a message wakes it early.

A session is never pinned and sunk at once, but the transitions are one-step in either direction: pinning a sunk row surfaces it, and a pinned row's menu still offers Archive and Snooze, either of which removes the pin (matching the TUI, no unpin-first needed). Bulk Archive and Snooze include pinned rows in the selection for the same reason. The "Snoozed & archived" section sits at the bottom and aggregates every sunk workspace; collapsed by default, its state persists in localStorage. The three menu entries are hidden in read-only mode.

### Multi-select and bulk triage

Select multiple rows to act on the whole selection:

- **Cmd/Ctrl+click**: toggle a row in or out of the selection without navigating.
- **Shift+click**: select every visible row between the last clicked and this one (collapsed groups and an active filter trim the range). The row you most recently plain-clicked or toggled is the anchor, so plain-click one row then Shift+click another to select the range between them.
- **Plain click**: clears the selection and opens that session (it stays the anchor for the next Shift+click).

With more than one row selected, right-click (long-press on touch) any selected row to act on the whole selection: the menu opens with a **N selected** header and triage actions split by the rows they affect (e.g. **Pin 3** alongside **Unpin 2**). Right-clicking a row that is not in the selection resets the selection to that row and shows its single-row menu. Bulk actions are best-effort: each session updates independently, a summary is reported, then the selection clears. Clear the selection with **Escape** or by plain-clicking a row. The selection survives collapse and filter changes but is not saved across reloads. Bulk actions are hidden in read-only mode.

## Projects

A **Projects** section near the bottom of the sidebar (above "Snoozed & archived") lists your saved projects that are not pinned and have no live session. It is the same registry the new-session wizard's "Saved projects" tab reads, and it shows on every grouping axis. The section is expanded by default; its collapsed state persists per-browser.

Pinned projects are not in this section: a pinned project always renders above as its own header, with or without sessions (pin from a project header's right-click menu). A project that has live sessions also renders above as its normal group. So the section is the home for the "saved but neither pinned nor currently in use" projects, which previously lived only on the standalone Projects page (now removed).

- **Add** with the **+** on the section header: browse for a path or type one, optionally set a name and a default base branch, choose global or profile scope, then save. The project appears in the section, ready to start work in.
- **Start a session** by clicking a project row (or its **+**), which opens the new-session flow in that repo. Once it has a session it moves up into the normal group list.
- **Edit base branch** and **Remove** live in the row's right-click (long-press on touch) menu. Removing deletes every registration for that path. The add / edit / remove controls are hidden in read-only mode.

## Profiles

The **Profiles** tab in Settings (`/settings/profiles`, the first entry in the Settings sidebar) manages configuration profiles: a left rail lists every profile with a **default** badge; the detail panel lets you create, rename, delete, set the default, and edit a description. **Edit configuration** buttons deep-link into the matching Settings tab scoped to that profile (`/settings/<tab>?profile=<name>`). The old `/profiles` URL redirects here.

Lifecycle hooks are shown **read-only** here, each labeled with its source (profile override, an override disabling inherited commands, inherited global commands, or none). Hooks run arbitrary shell commands, so they are never writable from the web; edit them in your config file or the TUI. The same applies to the agent-command and environment fields. In read-only mode the create / rename / delete / set-default / description controls are hidden.

## On mobile

Below the `md` breakpoint the dashboard shows a single full-viewport pane instead of the desktop split. The right-panel button opens a picker that swaps the main pane between **Agent terminal**, **Diff**, and **Paired terminal**; a back chip in the diff and paired views returns to the agent terminal. The agent terminal and paired shell stay alive in the background when you switch away, preserving scrollback and focus.

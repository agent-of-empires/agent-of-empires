# Dashboard & Workspaces

The dashboard is the home screen of the web app: a workspace sidebar on
the left, the active session in the main pane, and a top bar with global
actions. This page covers the layout, how to create a session, and how
to keep a long session list under control. For running the server and
auth, see the [Web Dashboard overview](../web-dashboard.md).

![The dashboard with the workspace sidebar, session summary, and status glyphs](../../assets/web/dashboard.png)

## Layout

- **Workspace sidebar** (left) lists every session grouped by repo, with
  a live status glyph per row. On phones it collapses behind a toggle in
  the top bar.
- **Main pane** shows the selected session: the agent terminal (or
  cockpit view), with the diff and paired terminal reachable from the
  top bar.
- **Top bar** carries the command-palette trigger, the right-panel
  picker, and the overflow (three-dot) **More options** menu.
- **Home screen** (no session selected) shows the AoE logo and a summary
  of how many sessions are running, waiting, or in error.

### Status glyphs

Each sidebar row carries an animated braille glyph that encodes the
session's state: a spinner of dots while **Running**, an orbiting dot
while **Waiting** or **Creating**, and a slow breathe while **Starting**
or freshly idle. Errors render in the error color. The animation frame
is offset by each session's creation time so a wall of rows does not
pulse in lockstep.

## Creating a session

The **New session** wizard walks four steps: project, session, agent,
and review.

- **Project** picks the working directory from your recent and
  registered projects, or starts a scratch session with no path.
- **Session** sets the title, which auto-slugifies into a worktree
  branch name unless you edit the branch directly. You can attach an
  existing branch instead of creating one.
- **Agent** selects the tool and profile, and exposes the per-session
  knobs: auto-approve (YOLO) mode, "Run in a safe container" (sandbox),
  command override, and extra args / env.
- **Review** confirms the configuration before the session spawns.

Choosing a profile seeds the agent-step defaults. If you have already
edited a field, switching profiles asks before overwriting your changes,
so a late-arriving profile default cannot clobber what you typed.

## Command palette

The command palette (triggered from the top bar or its keyboard
shortcut) is a fuzzy launcher for global actions: jump to a session,
open settings, start a new session, toggle the right panel. It is the
fastest path to anything the dashboard can do without reaching for the
mouse.

## First-run tutorial

The first time you open the dashboard in a browser, an interactive walkthrough launches automatically and highlights the major regions: the command bar, the workspace sidebar, how to start a session, settings, and (inside a session) the diff panel and composer. Each step lists the keyboard shortcuts that apply to it, and every step has a **Skip** button so you can dismiss the whole tour in one click.

Completing or skipping the tour records that you have seen it (a per-browser `aoe-tour-seen` flag in `localStorage`), so it does not launch again on reload. Because the flag is per origin, a debug build on port 8081 and a release build on port 8080 each track it separately.

To replay it at any time, open the overflow menu (the three-dot **More options** button in the top bar) and choose **Show tutorial**. Re-triggering it adapts to where you are: on the dashboard it covers the dashboard regions; inside a session it also covers the composer, agent mode picker, and send/queue controls. The tutorial does not auto-launch on touch devices, where it is available only from the menu.

## Sidebar sort

By default the sidebar shows your manually-ordered list. Drag a row with a press-and-hold gesture to move it; the new order persists across browsers and devices via `workspace-ordering.json`.

To reorder whole projects, grab the drag handle on the left of a project/group header and drag it up or down. This sets an explicit group order instead of leaving project placement to be derived from whichever session sits highest. Unlike row ordering, the group order is per-browser (localStorage), not synced across devices. A project that appears after you have set an order slots in at the top. The Multi-repo and Scratch groups default to the bottom but are draggable too, so you can lift them anywhere; once dragged they hold their chosen spot. Group drag is disabled while a filter is active or while Recent activity sort is selected, since the order is computed in those cases.

A sort toggle next to the filter button in the sidebar header switches to **Recent activity** mode, which orders workspaces by the most recent of `last_accessed_at`, `idle_entered_at`, and `created_at` across each workspace's sessions, descending. Drag-to-reorder is disabled while Recent activity is selected, because the order is computed; the press-and-hold gesture does nothing in that mode.

The toggle's state is per-browser (localStorage), not synced across devices and not tied to your profile. Toggling back to manual restores the stored manual order and re-enables drag. The Multi-repo group defaults to the bottom; in manual mode you can drag it anywhere and it holds that spot, while in Recent activity mode group drag is disabled and it stays at the bottom.

## Sidebar grouping: by repo, by group, or both

A grouping toggle (the layers icon) next to the sort toggle cycles the axis the sidebar organises sessions by. Each click advances through the three modes, **By repo** to **By group** to **By repo and group** and back:

- **By repo** (default) groups workspaces by their git repository, the original behavior.
- **By group** groups sessions by the user-defined group you assigned in the TUI rename dialog, with `aoe group move`, or from the web sidebar itself (see **Edit group** below), mirroring the TUI's group headers. Sessions with no group fall into an **Ungrouped** bucket pinned to the bottom. A session whose worktree hosts agents in different groups shows up under each of those groups.
- **By repo and group** keeps the repository headers and nests the user groups inside each one, so a single repo block shows its `feature`, `fix`, and **Ungrouped** subgroups with the matching sessions under each. This is the mode to use when you want repo-level focus and group-level structure at the same time. A session split across groups appears once per subgroup, sliced to that group's sessions.

The choice is per-browser (localStorage). Collapse state is tracked separately for each axis, so collapsing a group in **By group** does not collapse a repo in **By repo**. In **By repo and group**, the repository headers share their collapse state with **By repo**, while each nested subgroup collapses independently and is keyed per repository, so collapsing `feature` in one repo leaves `feature` in another repo expanded. You can move a session between groups from the web context menu, but group rename, color, and drag-reorder still live on the repo axis only, and groups themselves are not reorderable.

**Edit group.** Right-click (long-press on touch) a session row and choose **Edit group** to retag it: type an existing group to move the session there, type a new path to create that group, or clear the field to drop the session back to **Ungrouped**. Group paths use `/` for hierarchy (for example `work/projects`), matching the wizard's group field. The action is hidden in read-only mode.

## Triage: pin, archive, snooze

The sidebar exposes three triage primitives via the right-click (long-press on touch) context menu on any session row:

- **Pin** floats the workspace to the top of the sidebar in every sort mode (manual and Recent activity). Pin is web-only and intentionally distinct from the TUI's favorite mark, which is a within-tier signal for the Attention sort. The web pin renders as a pushpin glyph next to the row title; the TUI favorite keeps its `*` star marker.
- **Archive** kills the session's tmux pane (or shuts down the cockpit worker for ACP-cockpit sessions) and sinks the row into the collapsible "Snoozed & archived" footer of its repo group. Sending a message to the row from the dashboard wakes it back into the live list automatically. Daemon restarts and the cockpit worker reconciler both skip archived sessions, so a row stays parked until you explicitly unarchive it.
- **Snooze** sinks the row into the same footer for a chosen duration. The menu offers the same eight presets as the TUI snooze dialog: 1h, 2h, 3h, 4h, 5h, 6h, 1d, 1w. The row wakes automatically when the timer expires; sending a message wakes it early.

Snooze and archive are mutually exclusive with pin: pinning a sunk row surfaces it, and archiving or snoozing a pinned row removes the pin. The three primitives can be mixed freely across concurrent surfaces (TUI, CLI, web), and the data layer enforces the mutual-exclusion rules in one place so peer writes cannot leave a row in a contradictory state.

The "Snoozed & archived" section sits at the very bottom of the sidebar and aggregates every sunk workspace across all repo groups. It is collapsed by default; clicking the header expands the list and remembers the choice in localStorage. Drag-to-reorder is disabled on pinned and sunk rows since their placement is computed.

In read-only mode (`aoe serve --read-only`) the three menu entries are hidden, matching the existing read-only gate on Delete.

### Multi-select and bulk triage

To triage many sessions at once, select more than one row and act on the whole selection:

- **Cmd/Ctrl+click** a row toggles it in or out of the selection without navigating to it.
- **Shift+click** selects every row between the last clicked row and this one, in the order they appear. The range spans only visible rows: collapsed groups and a collapsed "Snoozed & archived" footer contribute nothing, and an active filter trims the range to matches.
- A plain click (no modifier) clears the selection and opens that session, the same as before.

While a selection is active, a bulk action bar appears above the list showing the count and the actions that apply. Actions split by the rows they can affect, so a mixed selection shows, for example, **Pin 3** alongside **Unpin 2** rather than one ambiguous toggle: **Pin**, **Archive**, and **Snooze** target the live rows; **Unpin**, **Unarchive**, and **Unsnooze** target the already-triaged rows. **Snooze** offers the same duration presets as the single-row menu.

Bulk actions are best-effort, not all-or-nothing: each session is updated independently, and the bar reports a summary (how many succeeded, failed, or were skipped) when the action finishes, then clears the selection. The selection survives collapsing a group or changing the filter and is dropped only when a session no longer exists; it is not saved across reloads. Like the single-row menu, the bulk bar is hidden in read-only mode.

## On mobile

Below the `md` breakpoint the dashboard shows a single full-viewport pane rather than the desktop side-by-side split. The right-panel button in the top bar opens a picker that swaps the main pane between three views: the **Agent terminal**, the **Diff** (changed files and review), and the **Paired terminal** (host or container shell). A back chip in the top-left of the diff and paired views returns you to the agent terminal.

Because each view owns the whole viewport, the paired terminal handles the soft keyboard the same way the agent terminal does. The agent terminal and the paired shell stay alive in the background when you switch away, so their scrollback and focus are preserved. The desktop side-by-side split is unchanged.

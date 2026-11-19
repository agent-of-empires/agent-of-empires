# Suggested sessions

Suggested sessions are things worth starting a session for, such as a PR that wants your review or a failing check, published by installed plugins. AoE only lists them. Nothing runs until you start one, and **Start** launches right away with the [preset](#presets) of the suggestion's kind.

Each item has a title, a reason, the plugin that proposed it, and a draft prompt. Plugins decide what to publish; this page covers using them. Writing a provider is covered in the [plugin contract](../plugin-api.md#suggested-sessions).

Suggestions show up in the web dashboard (homepage section, `/suggestions`, and optionally the sidebar) and in the TUI. They are stored per [profile](configuration.md#profiles).

## Setting up

With no provider installed, the web homepage and the empty `/suggestions` view offer **Set up suggestions** when a featured plugin exists. The sheet installs the sources you tick, runs each plugin's setup checks (✓, `!`, ✕, with the plugin's fix text and **Check again**), and ends with "Suggestions are on". **Finish later** closes it; the plugin stays installed. **More sources in the Marketplace ›** opens Settings › Plugins › Marketplace filtered to plugins that work with suggestions.

To skip the sheet, install a provider with `aoe plugin install` ([Plugins](../plugins.md)).

## Web dashboard

### Homepage and `/suggestions`

The homepage shows up to 3 suggestions (2 on a narrow screen) under "Suggested sessions", with **View all (N)** linking to `/suggestions`. Turn the section off in Settings › Suggestions.

The full view lists every suggestion, one line per row, grouped by repo:

- Two header rows: the title with a status line (the count and each provider's "Updated 2 min ago", "Waiting for first results", "Nothing new", "Refreshing…" or "Couldn't refresh: <error>"), **Refresh** and the gear for Settings › Suggestions; then **Search**, **All repos ▾** and **All kinds ▾**. Items from a failing provider stay listed from the last fetch.
- **Search** matches the title, source reference, reason and repo, ignoring case. **All repos ▾** lists the repos with counts. Keys of the form `org/repo` are grouped: the org row selects everything under that org, a repo row only that repository. **All kinds ▾** narrows to kinds such as Review requests. Selected rows combine, and **Clear filters** resets them.
- Source chips (**All**, one per source) appear only with two or more sources, and **Dismissed** once something is dismissed.
- Each repo group has a header with the count that collapses its rows. Rows show `#123` rather than `owner/repo#123`, then the reason once, such as "Review requested". A repo with no Project yet says so in its header.
- On a phone each row takes two lines, and **Start**, **▾** and ✕ keep a 44px tap area. **▾** opens as a bottom sheet.

### Starting one

**Start** runs the suggestion's kind [preset](#presets) at once: agent, view, worktree, sandbox, Yolo mode and the prompt, in the Project the repo is linked to. There is no New session dialog and no prompt review. A toast names the Session ("Started “<title>”") with **Open**, and the other rows stay usable meanwhile.

If the repo defines [hooks](repo-config.md) you have not approved, Start pauses on **Review this repo's hooks**. **Approve and start** trusts them and starts in one step. Start cannot skip this review, and it asks again when the hooks change.

**▾** next to Start shows what Start will use for this suggestion (Project, agent, view, worktree) and offers:

- **Start with changes**: opens the New session dialog prefilled from the preset, for this one only. A strip above the form names the source and says "issue text not included"; the source's own text is never added to the prompt for you. The dialog also shows "From the <kind> preset" with an **Edit preset** link.
- **Copy prompt**, **Open in <provider> ↗**, and **Edit the <kind> preset** (Settings › Suggestions).

If the repo has no Project yet, **Start in… ▾** replaces Start. Picking a Project links the repo to it, then starts, so later suggestions from that repo start in one click. It is off on read-only dashboards and in CityHall mode, which cannot save the link.

### After starting

The row stops offering Start. It shows "⠋ Running · started 3 min ago" (or "Idle", the session's other status, or "Archived") and an **Open** button. The **⋯** menu has **Start another Session…**, **Open in <provider> ↗** and **Dismiss**. Trashing the session removes the link; archiving keeps it. Only one Session can claim a suggestion: a second Start on it opens the first.

If a start fails, the row shows "Couldn't start: <reason>" and what to do, such as "Docker isn't running. Start it, then try again." **Try again** repeats the same start, and **Start with changes** opens the New session dialog.

### Dismiss and restore

**Dismiss** (✕) hides an item. A bar offers **Undo** for 10 seconds; on the homepage it also offers "Stop suggesting <kind>", which turns that kind off. Dismissed items stay under the **Dismissed** chip, where ↶ **Restore** brings them back. A dismissal is remembered by provider and item, so it survives the plugin publishing the same item again.

### Stale and hidden items

A **stale** item is dimmed and only offers Dismiss: its source closed or changed. It disappears after the plugin has published it stale for 24 hours.

If a provider is switched off, its items are hidden and the view says "<provider> suggestions are hidden because <provider> is off in Settings › Suggestions." with a **Turn it on** link.

## Settings › Suggestions

| Setting | What it does |
|---|---|
| Show on the homepage | The homepage section. |
| Show in the sidebar | A collapsed **Suggested (N)** group at the bottom of the web session sidebar, above Trash, absent when there are no open suggestions. Started suggestions are not listed, since the Session is already under its Project. Expanded, it lists 3 items and **View all**; clicking an item starts it like **Start**. Off by default. |
| Show where each one comes from | Shows where each suggestion comes from, such as `owner/repo#123`, on web rows and before the title on TUI rows. On by default. |
| Sources | Drag to set which source comes first (Alt+↑/↓ on a focused row, a move-up button on a phone). Each has an on/off switch. A source that uses an agent says so, since it spends your agent usage. A source error can carry a fix button from the plugin. |
| *Source* · Kinds | One row per kind of suggestion, such as Review requests: what it starts with ("Claude Code · Structured view · worktree"), a switch to suggest it, and **Edit** for its [preset](#presets). |
| *Source* source settings | The plugin's own settings for its suggestions, such as the GitHub plugin's ignored organizations and repositories. The same settings, saved to the same place, also appear under Settings › Plugins. |
| Advanced › Check for new suggestions every | 5 min, 15 min, 30 min, 1 h or 2 h. A hint sent to plugins; they may refresh less often. |
| Advanced › Linked repos | **Edit** opens [Projects](#projects). |
| Advanced › Dismissed suggestions | **Show** opens the Dismissed filter. |

### Presets

Each kind of suggestion has a preset: what **Start** uses for every suggestion of that kind. **Edit** on a kind opens it with the same rows, in the same order, as the New session dialog: **Agent**, **View** (Structured or Terminal), **Worktree**, **Sandbox**, **Yolo mode** and **Prompt**. The Project is not part of a preset: it comes from the repo's link, so one preset works across repos.

Until you edit it, a preset follows your new-session defaults (Yolo mode included) and the plugin's own prompt, so a later change to the defaults flows through. **Save** stores the whole preset for the current Profile. **Reset to <source>'s settings** removes your edits.

The prompt is a template. `{name}` expands to the item's variable of that name; unknown names stay as typed. The editor lists the available variables and previews the result against a current item. An empty prompt starts the session with no prompt, so you type the first message. Kinds whose text comes from other people, such as Mentions and New issues, start the same way, so Start sends their text as the prompt. To read it first, use **▾ › Start with changes**.

### Projects

**Linked repos** pins a source key (for example a repository) to one or more registered Projects. **Start in…** makes the same link. Unlinked keys show "no Project yet". Rules for how a Project is resolved are in the [API reference](../plugin-api.md#project-mappings).

#### Mapping wizard

**Link repos automatically…** under Linked repos, lists every source key in the current suggestions that has no mapping and proposes a Project for it:

1. A Project whose git `origin` is the key (`owner/repo`, ignoring case) is proposed and checked.
2. Otherwise a Project whose name or folder equals the key's last part, ignoring case, `-` and `_`, is proposed unchecked.
3. Keys with no proposal are listed under "No match"; choosing a Project checks them.

Change any proposal with its select, then **Link N repos** stores every checked row as a mapping to that one Project. Saving is refused on read-only dashboards and in CityHall mode.

## TUI

> The TUI keys below are provisional until #4597 settles the key bindings.

**Home.** A collapsed **Suggested (N)** group sits above Trash and is absent when there are no open suggestions, unless a provider failed: the group stays and its preview shows the error. Enter expands it to 3 items and a "View all" row. Rows show the title with the source reference after it. On an item, Enter prepares it and opens the prefilled dialog, `z` (`Z` in strict mode) dismisses it, and `Ctrl+K` lists "Open in <provider>". `N` (`Ctrl+N`) starts a blank session.

**Full view.** `S` opens it (`Ctrl+S` in strict mode).

| Key | Action |
|---|---|
| `j` / `k` | Select |
| Enter | First press resolves the final prompt and shows it ("Review the prompt below, then press Enter to start."); the second opens the dialog. On a started item it opens the session |
| `N` | Start a new session for a started item |
| `z` | Dismiss or restore |
| `d` | Toggle the dismissed list |
| `r` | Ask providers to refresh, then reload the list. On a read-only dashboard it only reloads |
| PgUp / PgDn | Scroll the prompt |
| Esc | Back |

An item with no matching registered Project cannot be started ("No registered project matches this suggestion"), and a stale one says "No longer open at the source. Dismiss it, or wait for the next refresh."

**New-session dialog.** The dialog has no Prompt row. The first turn is one line above the footer: "Prompt from GitHub, not checked by AoE" for a suggestion, or "Prompt: none". `Ctrl+O` opens it in a small editor, which also shows the item's source text; `Esc` closes it and keeps your edits. `Ctrl+P` still opens the focused row's picker. The prompt is sent once the agent is ready (typed into a terminal session, or sent as the first turn of a [structured-view](../structured-view.md) one).

From the command line, `aoe add --prompt <text>` or `--prompt-file <path>` sets the same first turn. It is rejected for terminal sessions.

## Read-only dashboards and CityHall

A read-only dashboard lists suggestions but cannot change anything: "This dashboard is read-only. Start, Dismiss and Refresh are off." The same applies to the TUI view. Offline, suggestions stay visible from the last fetch ("Disconnected. Suggestions show from the last fetch; reconnect to start one.").

In [CityHall mode](web-dashboard.md#cityhall-client-mode) the suggestion routes that change anything (start, refresh, dismiss, source and preference changes, preset and mapping edits) are refused server-side, and plugin lifecycle is hidden, so setup and the Marketplace are unavailable. Reading suggestions stays allowed. The [API reference](../plugin-api.md#suggestions-api) lists the exact routes.

## Troubleshooting

- **Nothing appears, "waiting for first results".** The plugin has not published yet. Press Refresh; the first fetch usually takes under a minute.
- **"couldn't refresh, <error>".** The plugin reported a failure and the last items are shown. If a fix button is offered, use it, then Refresh.
- **Setup checks failing.** In the setup sheet, follow each failed check's fix text, then **Check again**.
- **A kind of suggestion is missing.** Its switch under Settings › Suggestions › Kinds, or the whole source, may be off.
- **Start is greyed out.** Read-only or offline, see above.

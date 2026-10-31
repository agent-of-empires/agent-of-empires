# Suggested sessions

Suggested sessions are things worth starting a session for, such as a PR that wants your review or a failing check, published by installed plugins. AoE only lists them. Nothing runs until you start one, and starting always goes through a prefilled new-session dialog unless you turn on [Start without review](#start-without-review).

Each item has a title, a reason, the plugin that proposed it, and a draft prompt. Plugins decide what to publish; this page covers using them. Writing a provider is covered in the [plugin contract](../plugin-api.md#suggested-sessions).

Suggestions show up in the web dashboard (homepage section and `/suggestions`) and in the TUI. They are stored per [profile](configuration.md#profiles).

## Setting up

With no provider installed, the web homepage and the empty `/suggestions` view offer **Set up suggestions** when a featured plugin exists. The sheet installs the sources you tick, runs each plugin's setup checks (✓, `!`, ✕, with the plugin's fix text and **Check again**), and ends with "Suggestions are on". **Finish later** closes it; the plugin stays installed. **More sources in the Marketplace ›** opens Settings › Plugins › Marketplace filtered to plugins that work with suggestions.

To skip the sheet, install a provider with `aoe plugin install` ([Plugins](../plugins.md)).

## Web dashboard

### Homepage and `/suggestions`

The homepage shows up to 3 suggestions (2 on a narrow screen) under "Suggested sessions", with **View all (N)** linking to `/suggestions`. Turn the section off in Settings › Suggestions.

The full view lists every suggestion:

- Filter chips: **All**, one per provider, and **Dismissed** once something is dismissed.
- One status line per provider: "updated 2 min ago", "waiting for first results", "nothing new", "refreshing…", or "couldn't refresh, <error>". Items from a failing provider stay listed from the last fetch.
- Items group under the registered Project they resolve to; items with none sit last under "No project".
- **Refresh** asks every provider to look again.
- The gear opens Settings › Suggestions.

### Starting one

**Start** opens the session wizard prefilled with the Project, tool, title and prompt. A strip above the form names the source and says "issue text not included". The source's own text is never added to the prompt for you. Review the **Prompt** row, then Launch.

When a [template](#templates) produced the prompt, the wizard also shows "From template: <criterion>" with an **Edit template** link.

If the item resolves to no registered Project, the wizard opens without one and you pick it. A [mapping](#projects) makes the choice automatic next time.

### After starting

The row stops offering Start. It shows "Session running", "Session idle" (or the session's other status, or "Archived") with its title and age, and an **Open** button. The **⋯** menu has **Start another Session…**, **Open in <provider> ↗** and **Dismiss**. Trashing the session removes the link; archiving keeps it.

If a start fails, the row shows "Couldn't start: <reason>" and the button becomes **Open wizard**.

### Dismiss and restore

**Dismiss** (✕) hides an item. A bar offers **Undo** for 10 seconds; on the homepage it also offers "Stop suggesting <kind>", which turns that kind off. Dismissed items stay under the **Dismissed** chip, where ↶ **Restore** brings them back. A dismissal is remembered by provider and item, so it survives the plugin publishing the same item again.

### Stale and hidden items

A **stale** item is dimmed and only offers Dismiss: its source closed or changed. It disappears after the plugin has published it stale for 24 hours.

If a provider is switched off, its items are hidden and the view says "<provider> suggestions are hidden because <provider> is off in Settings › Suggestions." with a **Turn it on** link.

## Settings › Suggestions

| Setting | What it does |
|---|---|
| Show suggestions on the homepage | The homepage section. |
| Show source reference | Shows where each suggestion comes from, such as `owner/repo#123`, after the provider name on the web and TUI rows. On by default. |
| Providers | Drag to order (Alt+↑/↓ on a focused row, a move-up button on a phone). Each has an on/off switch and a **Show** checkbox per kind of suggestion. A provider that uses an agent says so, since it spends your agent usage. A provider error can carry a fix button from the plugin. |
| Refresh every | 5 min, 15 min, 30 min, 1 h or 2 h. A hint sent to plugins; they may refresh less often. |
| Dismissed suggestions | **Show** opens the Dismissed filter. |
| Prompt templates and Projects | **Edit**, see below. |
| Start without review | See below. |

### Templates

Each kind of suggestion has a prompt template that replaces the plugin's draft prompt. `{name}` expands to the item's variable of that name; unknown names stay as typed. The editor lists the available variables and previews the result against a current item. **Reset to the plugin's default** removes your template. An empty template starts the session with no prompt, so you type the first message.

### Projects

**Project mapping** pins a source key (for example a repository) to one or more registered Projects. Unmapped keys show "no mapping: the wizard asks". Rules for how a Project is resolved are in the [API reference](../plugin-api.md#project-mappings).

### Start without review

When on, **Start** creates the session straight from the template and mapping, skipping the wizard. This is web dashboard only, and only for an item whose template is non-empty and that resolves to a Project; anything else opens the wizard. **Start another Session…** always opens the wizard.

## TUI

> The TUI keys below are provisional until #4597 settles the key bindings.

**Home.** A collapsed **Suggested (N)** group sits above Trash and is absent when there are no open suggestions. Enter expands it to 3 items and a "View all" row. On an item, Enter prepares it and opens the prefilled dialog, `z` (`Z` in strict mode) dismisses it, and `Ctrl+K` lists "Open in <provider>". `N` (`Ctrl+N`) starts a blank session.

**Full view.** `S` opens it (`Ctrl+S` in strict mode).

| Key | Action |
|---|---|
| `j` / `k` | Select |
| Enter | First press resolves the final prompt and shows it ("Review the prompt below, then press Enter to start."); the second opens the dialog. On a started item it opens the session |
| `N` | Start another session for a started item |
| `x` | Dismiss or restore |
| `d` | Toggle the dismissed list |
| `r` | Ask providers to refresh |
| `l` | Reload the list |
| PgUp / PgDn | Scroll the prompt |
| Esc | Back |

`r` only asks for a refresh ("Refresh requested. Press l to reload the list."). An item with no matching registered Project cannot be started ("No registered project matches this suggestion"), and a stale one says "No longer open at the source. Dismiss it, or wait for the next refresh."

**New-session dialog.** The dialog has a **Prompt** row for the first turn. A suggestion fills it, labelled with the provider and "not checked by AoE"; edit it freely. `Ctrl+P` on the row shows the item's source text. The prompt is sent once the agent is ready (typed into a terminal session, or sent as the first turn of a [structured-view](../structured-view.md) one).

From the command line, `aoe add --prompt <text>` or `--prompt-file <path>` sets the same first turn. It is rejected for terminal sessions.

## Read-only dashboards and CityHall

A read-only dashboard lists suggestions but cannot change anything: "This dashboard is read-only. Start, Dismiss and Refresh are off." The same applies to the TUI view. Offline, suggestions stay visible from the last fetch ("Disconnected. Suggestions show from the last fetch; reconnect to start one.").

In [CityHall mode](web-dashboard.md#cityhall-client-mode) the suggestion routes that change anything (refresh, dismiss, source and preference changes, template and mapping edits) are refused server-side, and plugin lifecycle is hidden, so setup and the Marketplace are unavailable. Reading suggestions stays allowed. The [API reference](../plugin-api.md#suggestions-api) lists the exact routes.

## Troubleshooting

- **Nothing appears, "waiting for first results".** The plugin has not published yet. Press Refresh; the first fetch usually takes under a minute.
- **"couldn't refresh, <error>".** The plugin reported a failure and the last items are shown. If a fix button is offered, use it, then Refresh.
- **Setup checks failing.** In the setup sheet, follow each failed check's fix text, then **Check again**.
- **A kind of suggestion is missing.** Its **Show** checkbox, or the whole provider, may be off.
- **Start is greyed out.** Read-only or offline, see above.

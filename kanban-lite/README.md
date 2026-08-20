# Kanban Lite for Agent of Empires

A read-only Kanban board view of your active Agent of Empires sessions.

## What it contributes

- **Settings page** (`Settings → Plugin pages → Kanban`): a board grouped by session status or by repository.
- **Row column** (`kanban_status`): a compact status cell on each session row in the sidebar.
- **Sort key**: sort the session list by Kanban status.
- **Filter facet**: filter the session list by Kanban status.

## Install locally

```sh
cd /path/to/agent-of-empires
aoe plugin install ./kanban-lite --yes
aoe serve --stop
aoe serve --daemon
```

Then open the web dashboard and navigate to `Settings → Plugin pages → Kanban`.

## Develop

```sh
cd kanban-lite
python3 -m venv --copies .aoe-build/venv
.aoe-build/venv/bin/pip install -e '.[test]'
.aoe-build/venv/bin/pytest -v
```

After code changes, update the installed plugin:

```sh
cd /path/to/agent-of-empires
aoe plugin uninstall dev.karl.kanban-lite
aoe plugin install ./kanban-lite --yes
```

(Manifest changes require a daemon restart; Python-only changes can be applied with `aoe plugin update` once the plugin is installed, but the current CLI requires an interactive terminal for that command.)

## Settings

- `default_grouping`: `status` or `repo`.
- `refresh_secs`: how often the worker polls AOE for session changes (default 30).
- `show_row_column`: show the status cell in the sidebar (default true).
- `excluded_statuses`: comma-separated list of status columns to hide from the board (default `stopped,unknown`).

## Known limitations

- The board is read-only. Session rows are not clickable because the worker cannot discover the dashboard URL and the dashboard rejects relative `href`s.
- Status mapping relies on the Rust `Status` enum's `Debug` string; a stable status enum from AOE would remove this fragility.
- Drag-and-drop and moving sessions between columns require core session-mutation RPCs that do not exist yet.

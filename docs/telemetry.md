# Telemetry

AoE can send **anonymous, opt-in** usage telemetry so maintainers can answer basic product questions: how many installs are active, how many sessions people keep open, which agents, models, and platforms matter, TUI versus web. It is **off by default**, carries no PII and no content, and honors `DO_NOT_TRACK`.

## What is sent

Only aggregate counts, never a stream of actions. The wire format is the closed schema in `src/telemetry/events.rs`. Three event kinds:

- **`process_start`** when the TUI or `aoe serve` boots: surface, aoe version, OS, CPU arch, and the version-health fields below.
- **`cli_usage`** from `aoe <subcommand>` runs: surface, version, OS, arch, and a count map of allowlisted subcommand names (`{add: 5, list: 2}`). It is accumulated on disk and flushed as one POST per install per day, with no argument, flag, or path attached.
- **`usage_snapshot`** from the TUI and `aoe serve`, on start, shutdown, and about every four hours: session counts by status and how many use a sandbox, the structured view, or yolo mode; peak concurrency; pinned, snoozed, and archived counts; how many sessions sit in the trash (counted only here, and excluded from the other counts); a per-substrate census (`local`, `worktree`, `workspace`, `sandbox`, `scratch`); per-agent and per-model-family counts; how many sessions were created since the last snapshot; which opt-in features are on and which surfaces were opened; coarse structured-view interaction counts (approvals resolved and their allow/deny mix, agent switches, plan-mode use, queued prompts); for `aoe serve`, coarse auth (`token` / `passphrase` / `none`) and exposure (`tunnel` / `tailscale` / `local`) enums; and a plugin census by source, naming only builtin and featured plugins.

Model names map to a coarse family vocabulary (`claude`, `openai`, `gemini`, ...), anything unrecognized becomes `other` and an absent model `unset`, so the raw model string never leaves your machine. A custom agent command is reported as `custom`. In practice this is a handful of sub-1 KB requests per active install per day.

**Version health.** `process_start` and `usage_snapshot` carry `data_schema_version` (a small integer), `update_status` (`unknown`, `current`, `patch_behind`, `minor_behind`, `major_behind`), and `update_releases_behind` (`unknown`, `current`, `one_behind`, `several_behind`). None is a version string, and both update fields come from the local update-check cache, so they trigger no network call.

## What is never sent

Prompts, file or project paths, session titles, branch names, group paths, custom command lines, raw model strings, hostnames, usernames, or anything derived from them. Deployment signals carry only the coarse enums above, never a tunnel name, hostname, token, or passphrase.

The install id exists only to count distinct installs: a random UUID generated locally on opt-in, never derived from hostname, username, MAC, or filesystem. It lives in an owner-only `<app_dir>/telemetry.json`, deliberately outside `config.toml` since people paste that into bug reports. Opting out deletes the file, and `aoe telemetry reset-id` rotates it, which makes the install count as a new one.

## Controlling it

- **CLI**: `aoe telemetry status | enable | disable | reset-id`
- **TUI**: Settings, System, Telemetry
- **Web**: Settings, Telemetry, or the one-time consent prompt on first load

New users see a telemetry pane in the first-run walkthrough; anyone who finished that walkthrough before telemetry existed gets a one-time opt-in popup.

Setting `DO_NOT_TRACK` to `1`, `true`, or `yes` suppresses telemetry absolutely: nothing is sent and no install id is generated, whatever the config says. Every surface shows that suppressed state.

## Backend

Opted-in events go to `https://telemetry.agent-of-empires.com/v1/ingest`, which re-sanitizes every field before folding it into aggregate counts. `AOE_TELEMETRY_ENDPOINT` overrides the target, so you can point it at a local sink and see exactly what is sent. Sends are best-effort with a ~2s timeout and no offline buffering; failures are swallowed and never block the tool. The web dashboard never posts directly, which would leak its IP and User-Agent: it reports local state to `aoe serve`, which does all sending.

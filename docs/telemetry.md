# Telemetry

Agent of Empires can send **anonymous, opt-in** usage telemetry so the
maintainers can answer basic product questions (how many installs are active,
how many sessions people keep open, which agents/models/platforms matter, TUI
vs web). It is **off by default**, carries no PII and no content, and honors
`DO_NOT_TRACK`.

## What is sent

Only when you opt in, and only aggregate counts (never a stream of actions). The
full wire format is the closed schema in `src/telemetry/events.rs`. Three event
kinds:

- **`process_start`** when the TUI or `aoe serve` boots: surface, aoe version,
  OS, CPU arch, and version-health signals (below).
- **`cli_usage`** from `aoe <subcommand>` runs: surface, version, OS, arch, and a
  count map of allowlisted subcommand names (e.g. `{add: 5, list: 2}`).
  Accumulated on disk and flushed as one POST per install per day. No argument,
  flag, or path is attached.
- **`usage_snapshot`** from the TUI and `aoe serve`, on start, shutdown, and
  about every 4 hours. A point-in-time summary of the install:
  - session counts by status (running / idle / errored), and how many use a
    sandbox, the structured view, or yolo mode,
  - peak concurrent sessions, and counts of pinned / snoozed / archived,
  - a per-substrate census (`local` / `worktree` / `workspace` / `sandbox` /
    `scratch`),
  - per-agent and per-model-family counts (e.g. `{claude: 3, codex: 1}`),
  - how many sessions were created since the last snapshot,
  - which opt-in features are on, which surfaces were opened, and coarse
    structured-view interaction counts (approvals resolved and their
    allow/deny mix, agent switches, plan-mode use, queued prompts),
  - for `aoe serve` only, coarse deployment enums: auth mode (`token` /
    `passphrase` / `none`) and exposure (`tunnel` / `tailscale` / `local`),
  - a plugin census: installed count per source (`builtin` / `featured` /
    `community` / `local`) and the active state of builtin and featured
    plugins by id. Unfeatured GitHub and local installs are counted by source
    but never named,
  - the version-health signals below.

Model names are mapped to a coarse family vocabulary (`claude`, `openai`,
`gemini`, ...); anything unrecognized becomes `other` and an absent model
becomes `unset`. The raw model string never leaves your machine. Likewise a
custom agent command is reported as `custom`. In practice this is a handful of
sub-1 KB requests per active install per day.

### Version health

`process_start` and `usage_snapshot` carry three coarse fields so maintainers
can see how current the install base is. None is a version string:

- `data_schema_version`: a small integer (the data-schema version this build targets).
- `update_status`: a coarse bucket, one of `unknown` / `current` / `patch_behind` / `minor_behind` / `major_behind`.
- `update_releases_behind`: one of `unknown` / `current` / `one_behind` / `several_behind`.

Both update fields are read from the local update-check cache; they never
trigger a network call and never include the latest version number.

## What is never sent

Prompts, file or project paths, session titles, branch names, group paths,
custom command lines, raw model strings, hostnames, usernames, or anything
derived from them. Deployment-mode signals carry only the coarse auth and
exposure enums above, never a tunnel name, hostname, `.ts.net` URL, token, or
passphrase. The install id is a random UUID generated locally on opt-in; it is
never derived from hostname, username, MAC, or filesystem.

## Anonymous install id

To count distinct installs, opt-in generates a random UUID stored in
`<app_dir>/telemetry.json` (owner-only), kept out of `config.toml` on purpose
since people paste config into bug reports. Opting out deletes the file. `aoe
telemetry reset-id` rotates it, which makes that install count as a new one in
the aggregate; only reset if you want to disassociate from prior counts.

## Controlling it

Telemetry is **off by default**. Turn it on or off in any surface:

- **CLI**: `aoe telemetry status | enable | disable | reset-id`
- **TUI**: Settings, System, Telemetry
- **Web dashboard**: Settings, Telemetry, or the one-time consent prompt on first load

New users see a telemetry pane in the first-run walkthrough; users who finished
the walkthrough before telemetry existed get a one-time opt-in popup.

### `DO_NOT_TRACK`

If `DO_NOT_TRACK` is set to `1` / `true` / `yes`, telemetry is suppressed
absolutely: nothing is sent and no install id is generated, regardless of the
config flag. Every surface shows this suppressed state explicitly.

## Backend

Opted-in events go to `https://telemetry.agent-of-empires.com/v1/ingest`, which
re-sanitizes every field as defense-in-depth before folding it into aggregate
counts. `AOE_TELEMETRY_ENDPOINT` overrides the target, so you can point it at a
local sink to see exactly what is sent. Sends are best-effort with a ~2s
timeout; failures are swallowed and never block the tool, and there is no
offline buffering. The web dashboard never posts directly (that would leak its
IP and User-Agent); it reports local state to `aoe serve`, which does all
sending.

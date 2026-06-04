# Telemetry

Agent of Empires can send **anonymous, opt-in** usage telemetry so the
maintainers can answer basic product questions (how many installs are active,
how many sessions people keep open, which agents/models/platforms matter, TUI
vs web). It is designed to be conservative: **off by default**, no PII, no
content, and it honors `DO_NOT_TRACK`.

## What is sent

Only when you opt in, and only aggregate counts. Three event kinds, all with a
closed, versioned schema (see `src/telemetry/events.rs`):

- **`process_start`** on boot of a long-running surface (`tui` / `serve`): the
  surface, aoe version, OS, CPU arch, and the version-health signals (see
  "Version health" below). The `tui` and `serve` surfaces emit one per launch
  (not throttled), so a restart is visible; a pathological crash-loop is absorbed
  by the gateway rather than a local cap.
- **`cli_usage`** from short-lived `aoe <subcommand>` invocations: the surface
  (`cli`), aoe version, OS, CPU arch, a window-start timestamp, and a
  `command_counts` map of allowlisted subcommand name to invocation count
  (e.g. `{add: 5, list: 2}`). Each `aoe` run is a separate
  short-lived process, so counts accumulate on disk and flush as one POST per
  install per day; the daily throttle means scripting `aoe` in a loop never
  floods the endpoint, and the count mix answers "which subcommands this install
  actually uses" rather than just "the CLI ran today". The map keys are the fixed
  clap subcommand set (`add`, `session`, `telemetry`, ...), filtered against an
  allowlist before sending, so no argument, flag, or path is ever attached;
  hidden internal commands are never counted.
- **`usage_snapshot`** from the TUI and `aoe serve`, on start and then about
  every 12 hours, with a small random jitter on the period so installs that boot
  together don't snapshot in lockstep. It is a point-in-time summary of the
  current install, never a stream of actions:
  - how many sessions exist and how many are running / idle / errored,
  - how many use a sandbox, the cockpit, or yolo mode,
  - how many sessions are currently pinned, snoozed, or archived (a
    point-in-time count of the session-organization states, not how often
    those actions were taken),
  - a per-substrate census: each session is classified into exactly one of
    `local` / `worktree` / `workspace` / `sandbox` / `scratch` (a closed
    five-way vocabulary), so the counts partition the session total and answer
    "of N sessions, how many are worktree vs local vs sandbox vs ...". All five
    keys are always present. This is orthogonal to the sandbox count above: a
    sandboxed worktree counts as `worktree` here yet still in the sandbox count,
    so the `sandbox` bucket means "sandboxed and not also one of the others",
    not "all sandboxed sessions",
  - a per-agent and per-model-family count (e.g. `{claude: 3, codex: 1}`),
  - how many sessions were created since the last snapshot, a trend counter so
    short-lived sessions that start and end between two snapshots are still
    counted (populated by `aoe serve`; the TUI reports `0`),
  - which opt-in features are turned on (see "Feature flags" below),
  - which surfaces were opened since the last snapshot, as a `usage_seen` map
    of allowlisted signal name to open-count (see "Usage signals" below). For
    the web dashboard and cockpit, a coarse client form-factor class is also
    reported per surface (`web_clients_seen` / `cockpit_clients_seen`, each a
    was-seen flag over `desktop` / `desktop_pwa` / `mobile` / `mobile_pwa`) so
    desktop, mobile, and installed-PWA usage can be told apart. It is never a
    per-device id: the snapshot's `os` / `arch` describe the daemon host, not
    the client, so without this a phone PWA talking to a desktop daemon looked
    like a desktop client. Only the coarse class is derived (from display-mode,
    pointer type, and viewport width); no user-agent string, screen size, or
    device model is read or sent,
  - for `aoe serve` only, how the daemon is deployed, decided once at launch:
    its auth mode (`token`, `passphrase`, or `none`) and its exposure mode
    (`tunnel` for a Cloudflare quick or named tunnel, `tailscale` for a
    Tailscale Funnel, or `local`). These are coarse enums only; the TUI reports
    neither, since it hosts no server,
  - the same version-health signals carried on `process_start` (see below).

### Version health

The `process_start` and `usage_snapshot` events carry three coarse
version-health fields so the maintainers can see whether the install base is on
recent, patched versions and how large the backward-compat support burden is.
None of them is a version string:

- `data_schema_version`: a small integer, the data-schema version this build
  targets (`migrations::CURRENT_VERSION`). Successful installs converge to it on
  startup, since a failed migration aborts boot.
- `update_status`: a coarse semver-distance bucket from the local update check,
  one of `unknown` / `current` / `patch_behind` / `minor_behind` / `major_behind`.
  `unknown` means no update check has been cached yet; it is never reported as
  `current`.
- `update_releases_behind`: how many cached releases are newer than the running
  build, one of `unknown` / `current` / `one_behind` / `several_behind`. Counted
  from the cached release list, so a fallback cache that only knows the latest
  release reports the conservative `one_behind`.

Both update fields are read from the local update-check cache; they never trigger
a network call and never include the latest version number, only the coarse gap.

In practice that is a handful of small (well under 1 KB) requests per active
install per day. There is no offline buffering, so a flaky network drops events
rather than building a backlog; the only retry is coarse (see "Failure
isolation" below).

Every agent and model string passes through a sanitizer
(`src/telemetry/sanitize.rs`) that coerces it to a fixed allowlist: a custom
agent command becomes `custom`, an unrecognized model becomes `other`. **Raw
commands, file paths, titles, branch names, group paths, and prompts are never
sent.**

### Model families and the `other` bucket

`model_bucket` maps a model string to a small, closed vocabulary of family
names (`claude`, `openai`, `gemini`, ...); anything it does not recognize
becomes `other`, and an absent model becomes `unset`. The raw model string
never leaves the sanitizer, so an internal or private model name can only ever
be counted as `other`, never revealed.

The family list is maintained by hand. Unlike the agent allowlist, which is
derived from the built-in agent registry, there is no in-repo source of model
names to generate it from, so adding a newly common public family is a
deliberate edit in `sanitize.rs`. The signal that the list has drifted is the
`other` rate in the `sessions_by_model_bucket` aggregate: when `other` climbs,
a popular family is missing and should be added. By design that is the only
discriminator for unknowns; no hashed or partial form of an unknown model name
is emitted, since model names are low-entropy and a hash would be reversible.

### Feature flags

The snapshot includes a small `features` map (allowlisted feature name ->
on/off) so we can see which opt-in features installs actually turn on. It is
driven by a registry in `src/telemetry/features.rs`: tracking a newly gated
feature is one entry there (name + how to read it from config), not a schema
change. The key set is fixed and the values are booleans, so a flag can never
carry a path or name, and the gateway forwards only this allowlisted shape.

The values reflect the **global** config (the install default), not any single
profile's effective config. It is an install-level default-adoption signal;
since sessions can run under arbitrary profiles whose overrides are not folded
in here, per-session usage is reported separately by the session counts above.

### Usage signals

The snapshot also includes a `usage_seen` map (allowlisted signal name ->
open-count) so we can see which surfaces installs actually use within a window,
for example `{web: 3, cockpit: 1}`. It is driven by a registry in
`src/telemetry/usage_signals.rs`: instrumenting a new surface is one entry there
(its short name), not a schema change. The key set is fixed and the values are
counts, so a signal can never carry a path or free text, and the gateway
forwards only this allowlisted shape. The web dashboard reports an open by
pinging `POST /api/telemetry/seen`; an unregistered name is rejected. The TUI
never hosts the web surfaces, so it reports the map zeroed.

## What is never sent

Prompts, file or project paths, session titles, branch names, group paths,
custom command lines, model strings, hostnames, usernames, or anything derived
from them. For `aoe serve`, the deployment-mode signals carry only the coarse
auth and exposure enums above: never a tunnel name, named-tunnel hostname,
`.ts.net` URL, auth token, or passphrase. The install id is a random UUID
generated locally on opt-in; it is never derived from hostname, username, MAC,
or filesystem.

## Anonymous install id

Counting distinct installs needs a stable id. On opt-in, aoe generates a random
`uuid::Uuid::new_v4()` and stores it in `<app_dir>/telemetry.json` (owner-only).
Updates to that file are serialized with an advisory lock (a `.telemetry.lock`
sidecar) so concurrent `aoe` processes (TUI, CLI, `aoe serve`) can't clobber each
other's writes. It is kept **out of `config.toml`** on purpose, since people
routinely paste their config into bug reports. Opting out deletes the file; `aoe telemetry
reset-id` rotates it. Resetting mints a brand-new id, so that install then
counts as a new one in the aggregate distinct-install and retention numbers;
only reset if you actually want to disassociate from prior counts.

## Controlling it

Telemetry is **off by default**. Turn it on or off in any surface:

- **CLI**: `aoe telemetry status | enable | disable | reset-id`
- **TUI**: Settings → System → Telemetry
- **Web dashboard**: Settings → Telemetry, or the one-time consent prompt shown
  on first load

New users also see a telemetry pane in the first-run walkthrough; users who
finished the walkthrough before telemetry existed get a one-time opt-in popup.

### `DO_NOT_TRACK`

If the `DO_NOT_TRACK` environment variable is set to `1` / `true` / `yes`,
telemetry is suppressed absolutely: nothing is sent and no install id is
generated, regardless of the config flag. Every surface shows this suppressed
state explicitly rather than silently ignoring it.

## Failure isolation

Sends are best-effort with a hard ~2s timeout and every error is swallowed
(logged only at `debug`, `target: "telemetry"`). Telemetry never blocks, stalls
on exit, or crashes the tool. There is no offline buffering.

A send counts as delivered only on a confirmed `2xx`: a transport error or a
non-success HTTP status (for example a rejected key or a schema rejection at the
gateway) is treated as a failure, not a silent success. Signals are not consumed
until delivery is confirmed, so a failed send does not silently drop them:

- the CLI `cli_usage` daily slot is claimed only on a confirmed send, and the
  accumulated `command_counts` are cleared only then, so a failed send leaves
  both the slot and the counts intact for the next invocation to retry (bounded
  to once per hour so a down endpoint cannot make every `aoe` invocation
  re-send), and a transient failure never drops a window of commands;
- the serve `usage_seen` open counts, the per-form-factor client maps, and the
  session-create counter are cleared only after a confirmed snapshot send,
  decremented by exactly what was reported, so a failed snapshot keeps them for
  the next one instead of losing that window's signal.

This is coarse, last-write retry, not a durable queue: periodic snapshots are
still point-in-time, and a snapshot identical to the last confirmed one is
deduped rather than re-sent.

## Backend

Opted-in events go to the collection gateway at
`https://telemetry.agent-of-empires.com/v1/ingest`. The gateway validates the
envelope and re-sanitizes every field as a defense-in-depth backstop, then
folds the payload into aggregate counts. `AOE_TELEMETRY_ENDPOINT` overrides the
target (point it at a local sink to see exactly what is sent). A compiled-in
`X-Telemetry-Key` header lets the gateway drop unkeyed drive-by traffic; it is
visible in the source, so it is noise-shedding, not authentication.

The web dashboard never posts to the gateway directly (that would leak the
browser's IP and User-Agent); it reports local state to `aoe serve`, which owns
the install id and does all sending.

**Schema contract.** The wire format is the flat, closed schema in
`src/telemetry/events.rs`, mirrored by the gateway. New fields must be counts,
booleans, or short identifier-like strings (and the allowlisted bucket maps:
per-agent, per-model-family, per-substrate, the `usage_seen` open counts, and the
`web_clients_seen` / `cockpit_clients_seen` form-factor maps, all keyed by short
identifiers with numeric or boolean values); the gateway drops free text, paths,
branch-name-like strings, and any nested object, so anything richer than a count
or flag will not survive ingest.

**Idempotency key.** The `process_start` and `usage_snapshot` events carry a
per-event `uuid`, a random v4 UUID minted once when the event is built. It is
distinct from the install id (stable per install) and `sent_at` (a per-emit
timestamp), and is stable across any redelivery of the same logical event. The
gateway forwards it as the PostHog event `uuid`, so a retried or redelivered
POST is recognized as the same event and deduped downstream rather than
double-counted. It is excluded from the in-process snapshot dedup fingerprint,
so two snapshots with identical content still compare equal.

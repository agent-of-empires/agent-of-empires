# Telemetry

Agent of Empires can send **anonymous, opt-in** usage telemetry so the
maintainers can answer basic product questions (how many installs are active,
how many sessions people keep open, which agents/models/platforms matter, TUI
vs web). It is designed to be conservative: **off by default**, no PII, no
content, and it honors `DO_NOT_TRACK`.

## What is sent

Only when you opt in, and only aggregate counts. Two event kinds, both with a
closed, versioned schema (see `src/telemetry/events.rs`):

- **`process_start`** on boot: surface (`cli` / `tui` / `serve`), aoe version,
  OS, and CPU arch. The `cli` surface is throttled to at most once per install
  per day, so scripting `aoe` in a loop never floods the endpoint.
- **`usage_snapshot`** from the TUI and `aoe serve`, on start and then every
  ~12 hours: counts of sessions by status, how many are sandboxed / cockpit /
  yolo, counts bucketed by agent and by model family, and whether the web
  dashboard / cockpit was opened.

In practice that is a handful of small (well under 1 KB) requests per active
install per day, with no retries and no offline buffering, so a flaky network
drops events rather than building a backlog.

Every agent and model string passes through a sanitizer
(`src/telemetry/sanitize.rs`) that coerces it to a fixed allowlist: a custom
agent command becomes `custom`, an unrecognized model becomes `other`. **Raw
commands, file paths, titles, branch names, group paths, and prompts are never
sent.**

## What is never sent

Prompts, file or project paths, session titles, branch names, group paths,
custom command lines, model strings, hostnames, usernames, or anything derived
from them. The install id is a random UUID generated locally on opt-in; it is
never derived from hostname, username, MAC, or filesystem.

## Anonymous install id

Counting distinct installs needs a stable id. On opt-in, aoe generates a random
`uuid::Uuid::new_v4()` and stores it in `<app_dir>/telemetry.json` (owner-only).
It is kept **out of `config.toml`** on purpose, since people routinely paste
their config into bug reports. Opting out deletes the file; `aoe telemetry
reset-id` rotates it.

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

Sends are fire-and-forget with a hard ~2s timeout and every error is swallowed
(logged only at `debug`, `target: "telemetry"`). Telemetry never blocks, stalls
on exit, or crashes the tool. There is no retry, no offline buffering.

## Backend

The client is **backend-neutral**. The send endpoint is read from the
`AOE_TELEMETRY_ENDPOINT` environment variable; there is no hardcoded server.
With no endpoint configured, nothing is ever sent even when opted in. The web
dashboard never posts to the telemetry backend directly (that would leak the
browser's IP and User-Agent); it reports local state to `aoe serve`, which owns
the install id and does all sending.

The collection service lives in a separate repository and is wired in by
pointing `AOE_TELEMETRY_ENDPOINT` at it; until then opting in is a no-op beyond
generating the local install id.

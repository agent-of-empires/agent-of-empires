# Logging

Agent of Empires uses the [`tracing`](https://docs.rs/tracing) crate. The TUI, the `aoe serve` daemon, the structured view runner subprocesses, and env-overridden one-shot CLI invocations all share `src/logging.rs` so they agree on env-var resolution, default filter construction, the reloadable subscriber handle, and the sink resolver. Every process appends to the same configured log file (`~/.agent-of-empires/debug.log` by default) so a single tail covers an entire session.

## Targets

Targets use `<module>.<submodule>`. The default filter expands a level to a directive per top-level root, so `auth.token` inherits from `auth` without per-target config. The settings dropdown only enumerates `KNOWN_SUB_TARGETS` (`src/logging.rs`); code may emit under any sub-target, and raw EnvFilter directives go through the settings filter field or `PATCH /api/log-level`.

Top-level roots:

| Root | What lands here |
|------|-----------------|
| `agent_of_empires` | Default crate target. Library code emitting without `target:`. |
| `acp` | ACP transport, supervisor, event store, runner shim, per-tool-call entry/exit. |
| `terminal` | Web terminal WS relay + per-byte firehose (trace). |
| `auth` | Token rotate, middleware accept/reject, rate-limit, login. |
| `process` | Signal sends, process-tree walks, survivor reap, ppid resolution. |
| `update` | GitHub release polling, cache, version compare. |
| `containers` | Docker daemon, image pull, container lifecycle, exec. |
| `git` | `git` invocation args, exit, duration. |
| `migrations` | Per-migration progress with duration. |
| `web` | Browser-side events relayed via `/api/client-log` (`web.client`); client module preserved in `client_target`. |
| `cli` | One-shot CLI subcommand entry/exit and outcome (`cli.serve`, `cli.add`). |
| `tui` | Key dispatch, screen transitions, dialog lifecycle, sampled render diagnostics. |
| `session` | Session/profile/group CRUD, terminal capture, heartbeat, storage IO. |
| `tmux` | tmux invocations, cache refresh, status detection, pane CRUD. |
| `http` | Axum request span (`request_id`/`method`/`path`/`status`/`latency_ms`) + per-route events. |
| `serve` | `aoe serve` startup, PID/URL file IO, tunnel up/down, signal shutdown. |
| `hooks` | Agent hook install/uninstall, status-file lifecycle, hook command + watcher failures. |
| `sound` | Notification sound download/install and per-event playback. |
| `telemetry` | Opt-in usage telemetry (all `debug`); consent route emits under `http.api.telemetry`. |
| `log.runtime` | Filter swaps (REST + runner file-watch). A swap matching the active directive is a silent no-op, so a watcher write cannot re-trigger itself. |

## Levels

- **error**: user-visible failure or invariant violation
- **warn**: recovered from, but worth investigating (rate-limit lockout, SIGTERM survivor, garbled runtime-filter file)
- **info**: lifecycle / state transitions (token rotate, container start, migration completed)
- **debug**: frequent / per-operation detail (every git invocation, every signal)
- **trace**: per-byte / per-message firehose (`terminal.ws.bytes`, ACP JSON-RPC transport)

## Env variables

Resolved once at startup in `LogConfig::from_env`:

| Var | Effect |
|-----|--------|
| `AOE_LOG_LEVEL` | `trace`/`debug`/`info`/`warn`/`error`. Sets the level across all known target roots. |
| `AGENT_OF_EMPIRES_DEBUG` | Legacy alias for `AOE_LOG_LEVEL=debug`. |
| `AOE_ACP_TRACE` | Overlay: `agent_client_protocol=debug` + the JSON-RPC transport_actor at trace. |
| `AOE_TERMINAL_TRACE` | Overlay: `terminal=trace` (per-byte firehose). |

## Sinks by process

One sink resolver (`logging::resolve_sink`) picks where each process writes based on `ProcessContext` + `[logging]`:

| Invocation | Context | `output = "stdout"` honored? | Default sink |
|---|---|---:|---|
| Any `aoe` with env var set, single-command | `OneShotCli` | yes | configured file (default `debug.log`) |
| `aoe serve` (foreground) | `ServeForeground` | yes | configured file |
| `aoe serve --daemon` child | `ServeDaemonChild` | no (coerced to file) | configured file |
| TUI (`aoe` with no subcommand) | `Tui` | no (coerced to file) | configured file |
| Structured view runner subprocess | `Runner` | no (coerced to file) | configured file |
| Other one-shot CLI, no env | — | n/a | no subscriber |

The TUI, daemon child, and runner coerce because their stdout would corrupt the alt-screen, be detached to /dev/null, or be unreachable. Coercion surfaces a `log.runtime` warning at startup so users see why their `output = "stdout"` didn't apply.

The daemon's stdout/stderr are also redirected at the OS level to the same configured log file. That preserves panic backtraces (and any stray `println!`) alongside the structured tracing stream. An inherited file descriptor may go stale after a mid-run rotation; the daemon keeps writing to the rotated `.1` until restart. This is best-effort, not load-bearing.

## Persistent configuration (`[logging]` in `config.toml`)

The settings UI (web dashboard *Settings → Logging*, TUI *Settings → Logging*) writes a `[logging]` section to `~/.agent-of-empires/config.toml`:

```toml
[logging]
default_level = "info"

# Sink and rotation knobs (changes require restart):
output = "file"          # "file" | "stdout"
file_path = "debug.log"  # relative -> app_dir, absolute -> verbatim
rotation = "size"        # "size" | "never"
max_size_mib = 50
keep_count = 5

# Whether the formatter prefixes each event with the span chain wrapping
# it (e.g. `http_request{request_id=... method=GET path=...}` from the
# per-request middleware). Off by default keeps the log readable; turn
# on for grep-correlation when triaging across async boundaries.
# Restart required.
show_spans = false

[logging.targets]
"acp.protocol" = "trace"
"auth.middleware" = "debug"
"process.signal" = "warn"
```

`default_level` is the baseline; entries in `targets` override per target. The list of targets surfaced as dropdowns mirrors `KNOWN_SUB_TARGETS` in `src/logging.rs`. Anything else can still be set via raw EnvFilter syntax through the runtime endpoint or CLI.

**Hot-swap vs restart:** `default_level` and `targets` hot-swap through the same `FilterController` that powers `aoe log-level`, including propagation to structured view runners via the `runtime_filter` notify watcher. No daemon restart needed for filter changes. The sink-shape fields (`output`, `file_path`, `rotation`, `max_size_mib`, `keep_count`) are set once at process startup and require a restart to take effect; the settings UI labels them accordingly.

## Rotation

When `rotation = "size"`, crossing `max_size_mib` shifts the rename chain (`debug.log.N-1 → .N` up to `keep_count`, current `→ .1`) and opens a fresh `debug.log`; files past `keep_count` are dropped. `rotation = "never"` grows unbounded (useful for debug sessions).

TUI, daemon, runners, and env-overridden CLI may all append concurrently. Three mechanisms keep that safe:

1. **`fs2` advisory exclusive lock** on `{path}.lock` serializes the rename chain. The OS releases it on exit, so a crashed rotater never wedges future rotations.
2. **Inode tracking + reopen:** each writer re-stats every 16 KiB; an inode mismatch (another process rotated) triggers a reopen so writes land in the new file, not the archived `.1`.
3. **Line-buffered writes:** only complete lines (`\n`, or 8 KiB without one) pass the rotation check, so a rotation never splits an event across files.

## Startup marker

`init_subscriber` writes a raw line to the sink before tracing takes ownership:

```
2025-08-15T... INFO log.runtime [AOE_START_MARKER] version=1.7.0 pid=12345 exe=/usr/local/bin/aoe
```

This is filter-immune (it bypasses the tracing subscriber entirely) so it survives any `default_level` setting and gives forensic readers a hard boundary between process runs. A parallel `tracing::info!(target: "log.runtime", "aoe started")` is also emitted once the subscriber is live; it respects the user's filter.

When the sink is stdout (foreground `aoe serve` with `output = "stdout"`, or env-overridden one-shot CLI), the marker is written to stdout too. Any tool piping or capturing the output will see the line before any structured event. That is intentional, so a captured stream is grep-compatible with a captured log file.

The TUI serve dialog uses captured file-offset-before-spawn (not the marker) to bound its tail pane, so the marker is a forensic / `grep` convenience rather than UI-load-bearing.

## Runtime control

### REST

| Method | Path | Body | Notes |
|--------|------|------|-------|
| `GET` | `/api/log-level` | — | `{current, reloadable, ephemeral}`. Returns 200 even when no controller is installed. |
| `PATCH` | `/api/log-level` | `{"level": "<name>"}` or `{"filter": "<EnvFilter>"}` | Exactly one field. |

`{"level": "debug"}` expands across all known target roots so you don't accidentally enable debug for transitive crates like `hyper`/`rustls`/`tower`.

`{"filter": "..."}` accepts the full `EnvFilter` syntax. Regex matching is disabled (`with_regex(false)`) to reduce attack surface on an authenticated HTTP input. Bare global levels (`"filter": "debug"`) are rejected with 400; use the `level` form instead.

Responses include `previous` and `current` directives. Changes are ephemeral; restart falls back to the env-var resolution.

### CLI

```
aoe log-level <level>             # safe expansion across known roots
aoe log-level --filter <expr>     # raw EnvFilter
aoe log-level --get               # print current
```

Examples:

```sh
aoe log-level debug
aoe log-level --filter acp.protocol=trace,info
aoe log-level --filter auth.rate_limit=debug,warn
aoe log-level --get
```

The CLI reads the daemon URL from `serve.url` and authenticates via the token in the query string. Works against a foreground `aoe serve` too; it does not require daemon mode.

## Runner propagation

`PATCH /api/log-level` makes the daemon write the new directive atomically to `~/.agent-of-empires/runtime_filter` (0600); each runner subprocess `notify`-watches the file and applies it to its own `FilterController`, so you can pull `info` to `trace` mid-incident without restart or losing agent state. Edge cases: missing file, runner no-ops; daemon stop does not revert runner filters (next start rewrites); garbled content, runner `warn`s and keeps its prior filter.

## Per-session tee

To make `aoe acp logs --session <id>` useful, the daemon's `SessionTeeLayer` (`src/acp/session_tee.rs`) mirrors each session-scoped event into `acp-workers/<id>.log` in addition to the shared `debug.log` (a copy, not a redirect). Events are attributed by their `session` field, inherited through the `acp_session` span scope when not set explicitly. Per-session writes use the same `SizeRotatingWriter` (10 MiB, keep 2); open writers are LRU-bounded (64) so the daemon doesn't leak fds. Daemon-only (the runner is single-session); `acp.tee`-target events are skipped to avoid re-entrancy; the tee sits below the same `EnvFilter`, so `aoe log-level` applies to per-session files too.

## Web client relay

Browser-side `window.onerror`, `unhandledrejection`, React `ErrorBoundary`, and explicit `reportError()` calls are batched and POSTed to `/api/client-log`. The server re-emits them through `tracing` at target `web.client` so they land in the same `debug.log` as everything else.

Throttle (frontend): token-bucket 10 cap, 10/s refill, batches flush every 2s / 20 entries / ~48 KB. `pagehide` and `visibilitychange === "hidden"` flush via `navigator.sendBeacon` with a JSON Blob so logs survive page navigation.

Caps (server): max 50 entries per batch (413 otherwise), message truncated to 4 KB, stack to 16 KB, dynamic-target field sanitised and capped at 64 characters. URL is sanitised client-side to drop the `?token=` query param before transmission.

Not captured (intentional, v1): `console.error`. Wrapping it produces noisy duplicates and recursion hazards; if you need it later, flag-gate it.

## File locations

| Path | When it's written |
|------|-------------------|
| `<configured file_path>` (default `~/.agent-of-empires/debug.log`) | Every process that installs a tracing subscriber (TUI, foreground / daemon `aoe serve`, structured view runner, env-overridden CLI). The daemon's stdout/stderr is also redirected here at the OS level for panic backtrace capture. |
| `<configured>.1` ... `<configured>.<keep_count>` | Rotated files, oldest at the highest number. |
| `<configured>.lock` | Idle `fs2` advisory lock file used to serialize rotation across processes. Always present after the first rotation; do not delete while any aoe process is running. |
| `~/.agent-of-empires/runtime_filter` | Atomically written on every successful `aoe log-level` swap; consumed by runner watchers. |
| `~/.agent-of-empires/acp-workers/<session-id>.log` | Per-session diagnostics surfaced by `aoe acp logs --session <id>`. The runner writes its startup marker and the agent's stderr here directly; the daemon additionally tees every session-scoped tracing event into it (see Per-session tee above). Size-rotated at 10 MiB, keep 2. |
| `~/.agent-of-empires/serve.log.legacy` | One-shot rename of the pre-consolidation `serve.log` by migration v007. Safe to delete once you've extracted any data you needed. |

On Linux, replace `~/.agent-of-empires` with `$XDG_CONFIG_HOME/agent-of-empires`. Debug builds use `~/.agent-of-empires-dev` to avoid colliding with an installed release.

The `aoe logs` viewer and the TUI serve dialog both call `logging::resolve_log_path` so the configured `file_path` is the single source of truth. `aoe logs --serve` and `aoe logs --all` were removed when `serve.log` was retired; the current default `debug.log` carries everything.

## Conventions

- Set `target:` explicitly when filtering granularity below the crate level matters. The default crate path (`agent_of_empires`) suffices for grab-bag logs.
- Use structured fields, not interpolated text: `tracing::warn!(target: "auth.rate_limit", ip = %addr, attempts = n, "lockout")` rather than `warn!("lockout for ip {addr} ({n} attempts)")`. Field-based filtering and grep both win.
- Don't log secrets. Token material is never logged; auth events carry a `reason` field instead. Git command args are redacted (`https://***@host/...`) before tracing emit.
- Add new top-level target roots to `DEFAULT_TARGET_ROOTS` in `src/logging.rs` so the runtime control's `{"level": ...}` expansion picks them up.

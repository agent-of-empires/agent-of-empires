# HTTP API Reference

`aoe serve` exposes an HTTP API so external orchestrators (other agents, MCP tools, CI scripts) can drive sessions without attaching to a terminal. This page documents the orchestration endpoints; the web dashboard uses the same surface plus internal routes.

## Authentication

Protected routes follow the configured authentication mode. Token authentication accepts `Authorization: Bearer <token>`, a `?token=` query parameter, or the `aoe_token` cookie. Passphrase authentication uses `/api/login` and a device-bound `aoe_session` cookie. A daemon running `--auth none` needs no credential. Read-only mode (`--read-only`) answers write endpoints with `403 read_only`.

## Runtime CLI reads

Read-only CLI commands use `/api/runtime/read/ws` when `--daemon-url` or `AOE_DAEMON_URL` selects an HTTP endpoint. `/api/runtime/ws` is reserved for the TUI stream and does not serve this one-shot protocol.

An explicit endpoint requires a served answer and never falls back. Environment discovery falls back to local storage, with a notice on stderr, if its configuration is unusable or no TCP peer is reached within one second. Authentication, upgrade and protocol failures after reaching a peer never fall back. Each served exchange has a 15-second deadline.

After upgrading the CLI, a `protocol_mismatch` means the target daemon uses a different protocol version. On that daemon's host, run `aoe serve --restart` for a detached daemon; otherwise restart the foreground daemon or its service through the service manager. Update that host's installation if needed. A missing runtime-read route can also indicate an older daemon, but first check the selected URL and proxy routing.

Without an HTTP endpoint, Linux reads use a verified, same-user local daemon socket when published; otherwise they use local storage. Invalid or unverifiable publication artifacts refuse fallback. Other platforms use local storage.

The route follows the daemon's authentication mode. CLI clients use `AOE_DAEMON_TOKEN` or `AOE_DAEMON_PASSPHRASE`; passphrase login shares the ACP client's device-bound session cache. Environment URLs discard query parameters, including query tokens. Explicit endpoints reject queries. HTTP on a non-loopback host is accepted only without credentials; remote authentication requires HTTPS. Localhost connections bypass proxies and pin both loopback address families.

Loopback HTTP does not verify the listener's OS user. If another local user binds the selected port, that process can receive the CLI's bearer token or passphrase. Use it only with trusted local users and port ownership; unset the HTTP endpoint to use verified same-user UDS discovery on Linux.

Runtime snapshots include stored launch commands on both transports to preserve CLI output. Commands may contain inline secrets; treat clients allowed to read snapshots as trusted, and do not expose `--auth none` to untrusted peers. Human output from served reads strips C0/C1 control characters from daemon-provided fields; JSON retains the original values.

Wire protocol **version 4** sends a `Hello` followed by a `Snapshot`. Profiles carry `name`, `listed` and `aliases`; aliases select the same physical store while preserving the requested spelling. Alias-only external stores are available through scoped reads but excluded from ordinary profile inventories and all-profile lists.

Runtime-read session identity and parent relationships are scoped to a profile. The same session ID may appear in different profiles without invalidating the snapshot. Stored absolute project paths retain their spelling, including legacy `.` / `..` components and repeated separators; reads do not normalize them or resolve them on the client.

Profile stores and alias targets must remain responsive. A stalled filesystem operation can indefinitely block daemon status refresh and persisted session mutations waiting on the reload gate. A stalled snapshot worker holds `runtime.flight` and an admission permit; queued workers can exhaust read admission. Client exchange deadlines do not cancel these filesystem operations. Isolation of hung alias targets is tracked in [#4702](https://github.com/agent-of-empires/agent-of-empires/issues/4702).

Scoped session reads fail on unreadable profile data instead of returning an empty success. All-profile listing skips unreadable profiles. Contract-invalid stored sessions degrade only their profile's session data; invalid stored groups degrade only that profile's group enumeration. Invalid global projects degrade only global metadata. These components stay independent, and snapshot sampling never rewrites the stores.

Newly listed profile stores are read on a cache miss without replacing existing live session rows. A newly listed store reusing a cached profile name or alias is not admitted as a cache miss; the daemon must accept a reload first. This includes deleting and recreating a profile with the same name, preventing retained live rows from being mixed with replacement-store data.

Wire members declared nullable must still be present. CLI JSON is a separate contract; for example, `session show --json` omits an unknown `agent_session_id`. A `systemd_unit` glob in local list JSON describes an expected scope name, not proof that the unit exists; remote snapshots omit it.

Background daemon maintenance can recover missing native session IDs from validated publications after the agent pane exits. Read requests do not perform these writes.

## GET /api/sessions

Lists sessions, including trashed and archived ones. Pass `state` to filter server-side: `live` excludes trashed and archived sessions, `trashed` returns only trashed ones, and `all` (the default) filters nothing. An unrecognized value is rejected with `400` rather than ignored.

```bash
curl -sS -H "Authorization: Bearer $AOE_TOKEN" \
  "http://localhost:7777/api/sessions?state=live"
```

Each row carries `context_resume`, the request-invariant availability of preserving that agent's context across a later lifecycle transition. It is tagged by `state`, with a `reason` on every state but `available`:

| `state` | `reason` values | Meaning |
| --- | --- | --- |
| `available` | (none) | A resume target exists and the launch path will use it. |
| `indeterminate` | `runtime_check_required`, `agent_handshake_required` | The answer needs a runtime probe this endpoint does not perform: ask again at launch. |
| `unavailable` | `agent_unsupported`, `sandbox_unsupported`, `command_unsupported`, `forced_fresh`, `invalid_target`, `fork_pending`, `previous_failure`, `no_target` | Context will not be preserved. |

A daemon older than this field omits it, so treat an absent `context_resume` as unreported rather than `unavailable`.

### Status values

`status` is **PascalCase** everywhere the HTTP API reports it (`GET /api/sessions`, the create response, the `callback_url` payload). The CLI and `[status_hooks]` env vars use the lowercase form, so do not compare the two directly.

| Value | Meaning |
| --- | --- |
| `Creating` | Create is in progress, before `Starting`. |
| `Starting` | Created or restarted; the agent process is not up yet. |
| `Running` | The agent is working. |
| `Waiting` | The agent stopped and wants input. Treat as "needs a prompt". |
| `Idle` | The turn finished with no pending question. Treat as "task complete". |
| `Error` | The agent's pane reported an error. |
| `Stopped` | The tmux pane is gone (killed, exited, server restart). |
| `Deleting` | Delete is in progress. |
| `Unknown` | Status could not be determined. |

## POST /api/sessions

Creates a session. Pass `?wait=ready` to block until the new session's status leaves `Starting` (bounded at 10s); the response `status` reports whatever it actually reached, including `Error`, so a timeout does not mean success.

When `profile` is omitted, creation uses the server's selected profile or the configured default. An explicitly empty `profile` is rejected.

```json
{
  "path": "/path/to/repo",
  "tool": "claude",
  "title": "Fix Login Flow",
  "worktree_enabled": true,
  "create_new_branch": true
}
```

**Worktree fields.** `worktree_enabled` creates a managed worktree even with no branch name, deriving a safe branch from the resolved title; `worktree_branch` names one explicitly, and sending it alone still opts into worktree mode; `create_new_branch` chooses between creating a branch and attaching to an existing one.

**Dispatcher fields.** `callback_url` receives an HTTP POST when the session transitions to `Waiting`, `Idle`, or `Error`, so a dispatcher need not poll. It must be `http`/`https` and must not resolve to a loopback, private, or link-local address: that is checked at create time and re-resolved before every dispatch, with the approved address pinned for the request so a changed DNS answer cannot redirect it. Delivery is fire-and-forget; failures are logged, not retried. The body is `{"session_id", "old_status", "new_status", "at", "seq"}`, where `seq` is a per-process counter (reset on daemon restart) for discarding out-of-order deliveries. The URL is persisted with the session and never echoed back, but it is stored verbatim: prefer one carrying no credentials and authenticate deliveries another way.

`idempotency_key` (max 200 characters) makes a retry with the same key return the existing session as `200` instead of creating a duplicate, even across a daemon restart. A hard-deleted session releases its key.

## POST /api/sessions/{id}/send

Types a message into the agent and presses Enter, honoring the per-agent paste-burst delay. Before delivery, the HTTP endpoint removes bracketed-paste delimiters and remaining ESC bytes, then removes only trailing CR/LF characters. Internal line breaks, spaces, and tabs are preserved by this normalization. The terminal transport adapts pasted LF to CR and adds a final Enter to submit. An empty or whitespace-only result is rejected before delivery.

```bash
curl -sS -X POST -H "Authorization: Bearer $AOE_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"message":"summarize the failing test"}' \
  "http://localhost:7777/api/sessions/abc123/send"
```

| Status | Body | When |
| --- | --- | --- |
| `200` | `{"sent": true}` | Keys delivered to the tmux pane |
| `400` | `{"error": "message_empty"}` | Empty or whitespace-only message |
| `400` | `{"error": "acp_mode_unsupported"}` | Structured-view session, so no tmux pane |
| `403` | `{"error": "read_only"}` | Server is read-only |
| `404` | `{"error": "not_found"}` | No such session |
| `409` | `{"error": "session_not_running"}` | The tmux pane is gone |
| `409` | `{"error": "resume_failed", "message", "resume_session_id"}` | Auto-revive tried a stored conversation and the pane exited before AoE could prove the id invalid; the id is preserved for retry |
| `409` | `{"error": "session_transient", "status"}` | Mid-lifecycle, cannot accept input yet |
| `409` | `{"error": "session_archived" \| "session_trashed", "message"}` | The session is archived or trashed, even with a live pane or `revive: false`; unarchive or restore it first. `/start`, `/ensure`, `/container-terminal`, and structured-view prompts refuse the same way |
| `409` | `{"error": "sandbox_login_required", "message", "tool", "profile"?, "command"?}` | Reviving a sandbox needs the sandbox login of its agent (`sandbox.shared_credential = "separate_login"`); `tool` is the one to log in. `profile` is present only when the session runs under a profile other than the default one (pass it as `-p <profile>`); `command` only when the session runs its own command for the tool and `aoe sandbox login --tool <tool>` alone would log in another file than the launch waits for (pass it as `--command <command>`). Terminal sessions have no queue, so the message is not kept; send it again after `aoe sandbox login --tool <tool>` |
| `500` | `{"error": "tmux_error"}` / `{"error": "internal"}` | Logged server-side |

A structured-view prompt (`POST /api/sessions/{id}/acp/prompt`) that would revive a sandbox needing the login answers `409` with `{"error": "sandbox_login_required", "message", "tool", "profile"?, "command"?, "queued_id"}` instead (`profile` and `command` as in the `409` above): the prompt is kept in the session's queue under `queued_id` and delivered after the login, so do not send it again. The session also shows the instruction as its startup error until the login exists.

If the session's `host_hooks.before_start` hook fails while that launch is admitted, the prompt is kept the same way and the answer is `502` with `{"error": "before_start_hook_failed", "message", "queued_id"}`. The pending initial turn stays, and nothing is held: the queue is delivered by the next launch that works. `message` is fixed text (the hook failed, timed out, could not be started, or is still running for the session) and never carries the hook's command or output.

With Apple Container, which cannot run the login helper, `sandbox.shared_credential = "separate_login"` is refused instead of held: a launch that needs the login (`POST /api/sessions/{id}/acp/spawn`, `/start`, `/ensure`) answers `409` with `{"error": "sandbox_login_unsupported", "message"}`, where `message` is `sandbox.shared_credential is separate_login, which is not supported with Apple Container; use host_seed or Docker/Podman`. Nothing is held, kept in the queue or pushed, and the web app offers no login. Any other launch (a prompt that revives the session, the reconciler, a terminal send) reports the same message as a launch error, such as the session's startup error. A launch that does not use the shared credential, and `host_seed`, are not affected.

Concurrent POSTs to the same id are serialized, so two orchestrators racing on one session cannot interleave keystrokes; different ids run in parallel.

## GET /api/sessions/{id}/output

Snapshots the session's tmux pane. `lines` (default 200, clamped to 1..=2000) sets how many trailing lines to capture, and `format` is `text` (ANSI stripped) or `ansi` (raw pane bytes). It needs no write access, so it works under `--read-only`.

Responses are `200` with `{"id", "lines", "format", "content"}`, `400 format_invalid`, `404 not_found`, `409 session_not_running`, or `500`.

### Driving a session as a subagent

`send` and `output` are the minimum primitives for running a session as a controlled subagent:

1. `POST /api/sessions/{id}/send` with the prompt.
2. Poll until the session's `status` returns to `Idle` (cheaper than polling `output`), or until pane content stabilizes across two reads a second apart.
3. Read `output` once and capture its trailing region as the reply.

Status transitions also reach `callback_url` and push subscribers.

## MCP server

With `[web] mcp_enabled = true` in `config.toml` (read at daemon startup), the daemon serves a [Model Context Protocol](https://modelcontextprotocol.io) server over Streamable HTTP at `POST /mcp`. With the setting off the route does not exist. It takes the same token as the rest of the API and reports `401` without one.

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_sessions` | `state` (`live`, the default, or `all`), `group` (a group path, including everything beneath it), `children_of` (a session id) | The `GET /api/sessions` body, PascalCase `status` included. |
| `whoami` | none | `null` until sessions get their own credential. |

Both tools declare an `outputSchema` and return the result as `structuredContent`.

## Skills

AoE discovers Agent Skills packages from its managed store and from supported user-level agent directories. A skill is a directory with a valid `SKILL.md` carrying `name` and `description` frontmatter. External packages are read-only; adopt one to get an editable copy in the managed store.

Source roots are stable ids: `claude-user` (`~/.claude/skills`), `agents-standard` (`~/.agents/skills`), `gemini-user` (`~/.gemini/skills`), `opencode-user` (`~/.config/opencode/skills`), `kimi-legacy` (`~/.kimi-code/skills`), `prime-agent-user` (`~/.prime/agent/skills`), and `aoe-managed` (`<app-dir>/skills`).

| Endpoint | What it does |
| --- | --- |
| `GET /api/skills` | Every discovered skill plus the root registry. Same-named skills from different roots stay separate, source-qualified entries. |
| `GET /api/skills/{source}/{directory}` | One package, with its full `SKILL.md` in `content`. |
| `POST /api/skills` | Creates a managed skill from `{ directory, description }` and scaffolds its `SKILL.md`. |
| `PUT /api/skills/{directory}` | Replaces a managed skill's `SKILL.md` after validating its frontmatter and a 1 MiB limit. |
| `DELETE /api/skills/{directory}` | Deletes a managed package. External ones cannot be deleted through AoE. |
| `POST /api/skills/{source}/{directory}/adopt` | Copies an external package into the managed store, optionally under a `destination` name. |
| `POST /api/skills/sync` | Copies managed skills into the agents' own skills directories. |

A sync body takes `roots` and `directories` to narrow the reconcile (omit either to cover everything), plus `replace`, which names the skills AoE may take over. It returns one outcome per skill per root (`created`, `updated`, `unchanged`, `removed`, `conflict`, `error`) rather than stopping at the first conflict.

```json
{ "roots": ["claude-user"], "directories": ["review"], "replace": ["review"] }
```

A propagated copy carries an `.aoe-managed.json` marker naming its root, skill, and package digest. That marker is the only thing that lets AoE later replace or remove the directory, and only while the copy still matches the digest. So a skill you wrote by hand, or a propagated copy you edited, is reported as a `conflict` and never overwritten or removed, and automatic syncs never replace anything. This is also what makes AoE safe beside a symlink-based skill manager: a symlinked directory is something AoE did not deploy, so it is reported and left alone rather than followed. Replacing one moves the link aside instead of writing through it.

`skills.auto_propagate` runs the same sync at session launch for the agent being launched. It is off by default because it writes into your real agent config directories.

Every skill mutation needs a read-write server and an elevated session when login is enabled, and none are available in CityHall mode. Adoption rejects symlinks, special files, packages over 64 MiB, files over 32 MiB, more than 1,024 files, and nesting deeper than 16 levels.

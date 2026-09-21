# HTTP API Reference

`aoe serve` exposes an HTTP API so external orchestrators (other agents, MCP tools, CI scripts) can drive sessions without attaching to a terminal. This page documents the orchestration endpoints; the web dashboard uses the same surface plus internal routes.

## Authentication

Every endpoint requires the token `aoe serve` printed (also visible in the TUI's Serve panel), unless the server runs with `--no-auth`. Send it as `Authorization: Bearer <token>`, as a `?token=` query parameter, or as the `aoe_token` cookie. Read-only mode (`--read-only`) answers every write endpoint with `403 read_only`.

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

Types a message into the agent and presses Enter, like the TUI's send dialog and `aoe send`. Honors the per-agent paste-burst delay. `message` is sent literally; newlines become shift-Enter line breaks and a final Enter submits.

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
| `500` | `{"error": "tmux_error"}` / `{"error": "internal"}` | Logged server-side |

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

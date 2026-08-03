# HTTP API Reference

`aoe serve` exposes a small HTTP API so external orchestrators (other
agents, MCP tools, CI scripts) can drive sessions without attaching to
a terminal. This page documents the orchestration endpoints. The web
dashboard uses the same API surface plus additional internal routes.

## Authentication

All endpoints require a token unless the server was started with
`--no-auth`. The token is the one printed by `aoe serve` (or visible
in the TUI's Serve panel). Three transports are accepted:

| Transport | Example |
| --- | --- |
| Bearer header (recommended for clients) | `Authorization: Bearer <token>` |
| Query parameter | `?token=<token>` |
| Cookie | `aoe_token=<token>` (set automatically by the dashboard) |

Read-only mode (`aoe serve --read-only`) blocks every write endpoint
with `403 read_only`. Read endpoints work normally.

## GET /api/sessions

List sessions. Returns every session by default, including trashed and
archived ones; pass `state` to filter server-side instead of fetching
everything and filtering client-side.

**Query parameters**

| Name | Default | Notes |
| --- | --- | --- |
| `state` | (unfiltered) | `live` excludes trashed and archived sessions. `trashed` returns only trashed sessions. `all` (or omitting the param) is the historical unfiltered behavior. An unrecognized value is rejected with `400` rather than ignored, so a typo surfaces instead of silently returning every session. |

**Example**

```bash
curl -sS \
  -H "Authorization: Bearer $AOE_TOKEN" \
  "http://localhost:7777/api/sessions?state=live"
```

### Status values

The `status` field on each session is **PascalCase** on the wire, and the same
spelling is used everywhere the HTTP API reports a status: `GET /api/sessions`,
the `POST /api/sessions` response (with or without `?wait=ready`), and the
`callback_url` payload. Note this differs from the lowercase form the CLI and
`[status_hooks]` env vars use (`AOE_NEW_STATUS=waiting`), so a dispatcher
consuming both surfaces must not compare the two directly.

| Value | Meaning |
| --- | --- |
| `Starting` | Session was just created or restarted; the agent process is not yet up. |
| `Running` | Agent is actively working. |
| `Waiting` | Agent has stopped and is waiting for user input. This is the signal a dispatcher should treat as "needs a prompt". |
| `Idle` | The agent's turn has finished with no pending question. This is the signal a dispatcher should treat as "task complete". |
| `Error` | The agent's pane reported an error. |
| `Stopped` | The session's tmux pane is gone (killed, exited, server restart). |
| `Unknown` | Status could not be determined. |
| `Deleting` | Session delete is in progress. |
| `Creating` | Session create is in progress, before `Starting`. |

## POST /api/sessions

Create a session. The web dashboard uses this endpoint for the new-session
dialog, and external orchestrators may call it directly.

**Query parameters**

| Name | Notes |
| --- | --- |
| `wait` | Set to `ready` to block the response until the new session's status leaves `Starting` (or a 10s bound elapses), instead of returning immediately while the agent process is still coming up. The response `status` field reflects whatever the session actually reached, including `Error` if startup failed; a timeout does not mean success. |

**Worktree fields**

| Field | Notes |
| --- | --- |
| `worktree_enabled` | Set `true` to create a managed git worktree even when no explicit branch name is supplied. |
| `worktree_branch` | Optional explicit branch or worktree name. If omitted while `worktree_enabled` is true, AoE derives a safe branch name from the resolved session title. |
| `create_new_branch` | `true` creates a new branch; `false` attaches to an existing branch. |

For compatibility, callers that only send `worktree_branch` still opt into
worktree mode. To get title-derived branch names, send `worktree_enabled` as
`true` and omit `worktree_branch`.

**Dispatcher fields**

| Field | Notes |
| --- | --- |
| `callback_url` | An HTTP POST fires here when the session transitions to `Waiting`, `Idle`, or `Error`, so a dispatcher can react to completion without polling. Must be `http`/`https` and must not resolve to a loopback, private, or link-local address (rejected at create time, and re-resolved and re-checked before every dispatch; the approved address is then pinned for the request, so a DNS answer that changes in between cannot redirect it). Delivery is fire-and-forget: failures are logged server-side, not retried. The POST body is `{"session_id", "old_status", "new_status", "at", "seq"}`; `seq` is a per-process monotonic counter (resets on daemon restart) a dispatcher can use to discard an out-of-order delivery. |
| `idempotency_key` | A retry using the same key (even across a daemon restart, since the key is persisted on the created session) returns the existing session as `200` instead of creating a duplicate. Max 200 characters. If the originally-created session was later hard-deleted (not just trashed), the key is no longer found and a fresh session is created. |

`callback_url` is persisted with the session in the session store on disk, so
it survives a daemon restart. It is never echoed back in any API response, but
it is stored as given: prefer a URL that carries no credentials, and
authenticate deliveries by another means (for example a rotatable secret in the
path, or checking the `session_id` against the create you issued) rather than
embedding a bearer token in the query string.

**Example**

```json
{
  "path": "/path/to/repo",
  "tool": "claude",
  "title": "Fix Login Flow",
  "worktree_enabled": true,
  "create_new_branch": true
}
```

## POST /api/sessions/{id}/send

Type a message into the agent and press Enter, the same way the TUI's
send-message dialog and the `aoe send` CLI do. Honors the per-agent
paste-burst delay (e.g. Codex needs ~150 ms between text and Enter so
its burst-detection window expires before Enter arrives).

**Request body** (JSON)

```json
{ "message": "review the diff and pick the smallest fix" }
```

`message` is sent literally. Newlines inside the string are sent as
shift-Enter (line break in the agent's input box) and a final Enter
submits the whole message.

**Responses**

| Status | Body | When |
| --- | --- | --- |
| `200` | `{"sent": true}` | Keys delivered to the tmux pane |
| `400` | `{"error": "message_empty"}` | `message` is empty or whitespace-only |
| `400` | `{"error": "acp_mode_unsupported"}` | Session is structured-view/ACP mode and has no tmux pane |
| `403` | `{"error": "read_only"}` | Server is in read-only mode |
| `404` | `{"error": "not_found"}` | No session with that id |
| `409` | `{"error": "session_not_running"}` | Session exists but the tmux pane is gone |
| `409` | `{"error": "resume_failed", "message": "...", "resume_session_id": "..."}` | Auto-revive tried to resume a stored conversation, but the pane exited before AoE could prove the ID invalid. The ID is preserved for explicit retry or replacement. |
| `409` | `{"error": "session_transient", "status": "..."}` | Session is mid-lifecycle and cannot accept input yet |
| `500` | `{"error": "tmux_error"}` or `{"error": "internal"}` | Unexpected failure (logged server-side) |

Concurrent POSTs to the same `id` are serialized server-side, so two
orchestrators racing on the same session won't interleave keystrokes
inside the pane. Concurrent POSTs to *different* ids run in parallel.

**Example**

```bash
curl -sS -X POST \
  -H "Authorization: Bearer $AOE_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"message":"summarize the failing test"}' \
  "http://localhost:7777/api/sessions/abc123/send"
```

## GET /api/sessions/{id}/output

Snapshot of the session's tmux pane. Use this after `send` to read
what the agent printed back, or as a polling read-only view.

**Query parameters**

| Name | Default | Notes |
| --- | --- | --- |
| `lines` | `200` | Number of trailing lines to capture. Clamped to `1..=2000`. |
| `format` | `text` | `text` strips ANSI escape sequences. `ansi` returns the raw pane bytes (use this if your client renders color). |

**Responses**

| Status | Body | When |
| --- | --- | --- |
| `200` | `{"id": "...", "lines": N, "format": "text", "content": "..."}` | Pane captured |
| `400` | `{"error": "format_invalid", "allowed": ["text", "ansi"]}` | `format` was something other than `text` or `ansi` |
| `404` | `{"error": "not_found"}` | No session with that id |
| `409` | `{"error": "session_not_running"}` | Session exists but the tmux pane is gone |
| `500` | `{"error": "tmux_error"}` or `{"error": "internal"}` | Unexpected failure |

`output` does not require write access, so it works under
`--read-only`.

**Example**

```bash
curl -sS \
  -H "Authorization: Bearer $AOE_TOKEN" \
  "http://localhost:7777/api/sessions/abc123/output?lines=80&format=text"
```

## Driving a session as a subagent

Together, `send` and `output` are the minimum primitive needed to run
an aoe session as a controlled subagent. A typical loop:

1. `POST /api/sessions/{id}/send` with the prompt.
2. Poll `GET /api/sessions/{id}/output` until the pane content
   stabilizes (no change between two reads spaced ~1 s apart) or the
   session list shows the session's `status` back at `Idle`.
3. Capture the trailing region of `content` as the agent's reply.

For long-running prompts, prefer polling status via
`GET /api/sessions` over polling `output`, then read `output` once
when status returns to `Idle`. Status transitions are also broadcast
to push subscribers if the dashboard's push notifications are
configured.

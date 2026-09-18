# HTTP API Reference

`aoe serve` exposes a small HTTP API so external orchestrators (other
agents, MCP tools, CI scripts) can drive sessions without attaching to
a terminal. This page documents the orchestration endpoints. The web
dashboard uses the same API surface plus additional internal routes.

## Authentication

All endpoints require a token unless the server was started with
`--no-auth`. The token is the one printed by `aoe serve` (or visible
in the TUI's Remote Access view, `R`). Three transports are accepted:

| Transport | Example |
| --- | --- |
| Bearer header (recommended for clients) | `Authorization: Bearer <token>` |
| Query parameter | `?token=<token>` |
| Cookie | `aoe_token=<token>` (set automatically by the dashboard) |

Read-only mode (`aoe serve --read-only`) blocks every write endpoint
with `403 read_only`. Read endpoints work normally.

Five wrong tokens from one IP within 15 minutes lock that IP out for 15
minutes: every request then gets `429 rate_limited` with `Retry-After` in
seconds. A request that presents no token does not count. Wrong pairing codes
have their own lockout (see [POST /api/pair](#post-apipair)).

A device-bound login session also authenticates, with no token: send
`Cookie: aoe_session=<session_id>` plus `X-Aoe-Device-Binding: <secret>` (on
a WebSocket upgrade the binding may instead ride as the subprotocol
`aoe-device.<secret>`). Passphrase login and device pairing both mint one.
Sessions are listed by `GET /api/devices` and revoked with
`DELETE /api/login/sessions/{id}` or `POST /api/login/logout-all`; rotating
the token leaves them valid.

## Device pairing

### POST /api/pair/codes

Mints a single-use pairing code valid for 10 minutes. Only a caller on the
daemon's owner-checked Unix socket may mint; any TCP caller, loopback
included, gets `403 forbidden`. At most five codes are live at once (the
oldest is dropped), and codes do not survive a daemon restart.

```json
{"code": "K7F-3QX", "expires_in_secs": 600}
```

### POST /api/pair

Redeems a code. Needs no token or session.

```json
{"code": "K7F-3QX", "device_name": "laptop", "device_binding_secret": "<base64url of 32 random bytes>"}
```

Codes are case-insensitive and ignore `-` and spaces. On success the code is
consumed and the response carries a session presented as described under
[Authentication](#authentication), and `server_name`, the daemon machine's
hostname (`null` when unknown), which `aoe remote add` uses as the default
remote name. It passes a `--passphrase` login wall and
survives passphrase changes; `GET /api/devices` lists it with its
`device_name`.

```json
{"session_id": "<64 hex chars>", "device_name": "laptop"}
```

| Status | Meaning |
| --- | --- |
| `400 bad_request` | Malformed code, missing `device_name`, or a binding that is not 32 bytes |
| `401 invalid_code` | Unknown, used or expired code. Five from one IP lock it out of pairing for 15 minutes; 20 in total invalidate every live code |
| `429 rate_limited` | The caller's IP is locked out of pairing (`Retry-After` gives seconds) |

Only wrong codes count toward the pairing lockout, so other failed requests
from the same machine cannot block it. A successful pairing also lifts the
IP's token lockout.

### GET /api/pair/lockouts

Lists IPs locked out by wrong tokens or pairing codes, longest remaining
first. Unix socket owner only, like minting.

```json
[{"ip": "100.89.98.53", "remaining_secs": 725}]
```

### DELETE /api/pair/lockouts

Lifts every lockout (`204`). Unix socket owner only.

## Skills

AoE discovers Agent Skills packages from its managed store and supported
user-level agent directories. A skill is a directory containing a valid
`SKILL.md` with `name` and `description` YAML frontmatter. External packages are
read-only. Adopt one to create an editable copy under AoE's managed store.

Physical source roots are stable ids:

| Source id | Directory | Consumers |
| --- | --- | --- |
| `claude-user` | `~/.claude/skills` | Claude, OpenCode |
| `agents-standard` | `~/.agents/skills` | Codex, OpenCode |
| `gemini-user` | `~/.gemini/skills` | Gemini |
| `opencode-user` | `~/.config/opencode/skills` | OpenCode |
| `kimi-legacy` | `~/.kimi-code/skills` | Kimi legacy installations |
| `prime-agent-user` | `~/.prime/agent/skills` | Prime Agent |
| `aoe-managed` | `<app-dir>/skills` | AoE-managed packages |

### GET /api/skills

Returns every discovered skill plus the external root registry. Skills with the
same directory name remain separate source-qualified entries.

```json
{
  "skills": [
    {
      "directory": "review",
      "name": "Review",
      "description": "Review code carefully",
      "provenance": { "kind": "external", "root": "claude-user" },
      "provenanceLabel": "external:claude-user",
      "writable": false
    }
  ],
  "roots": [
    {
      "id": "claude-user",
      "label": "Claude",
      "relativePath": ".claude/skills",
      "consumers": ["claude", "opencode"],
      "primaryAgent": "claude",
      "legacy": false
    }
  ]
}
```

### GET /api/skills/{source}/{directory}

Reads one source-qualified package and returns its full `SKILL.md` in the
`content` field. Use `aoe-managed` for a managed skill or one of the external
source ids above.

### POST /api/skills

Creates a managed skill and scaffolds its `SKILL.md`.

```json
{ "directory": "release-check", "description": "Validate a release candidate" }
```

### PUT /api/skills/{directory}

Replaces a managed skill's `SKILL.md` after validating its frontmatter and
1 MiB size limit.

```json
{ "content": "---\nname: release-check\ndescription: Validate a release candidate\n---\n" }
```

### DELETE /api/skills/{directory}

Deletes a valid managed skill package. External packages cannot be deleted
through AoE.

### POST /api/skills/{source}/{directory}/adopt

Copies an external package into the managed store without changing the source.
The optional `destination` field changes the managed directory name.

```json
{ "destination": "team-review" }
```

### POST /api/skills/sync

Copies managed skills into the agents' own skills directories, so a skill
authored once in AoE is available to every agent. Pass `roots` to limit the
sync; omit it to reach every root.

```json
{ "roots": ["claude-user", "gemini-user"], "directories": ["review"], "replace": ["review"] }
```

`directories` narrows the sync to those skills, leaving every other one and
its copies alone; omit it to reconcile the whole store.

`replace` names skills AoE should take over, overwriting a skill it does not
manage or a propagated copy that was edited in place. It is the only way past
the never-overwrite rule below, so it must name each skill explicitly; an
omitted or empty `replace` overwrites nothing. A replaced entry becomes
AoE-owned, so later syncs keep it current on their own. Replacing a symlinked
entry moves the link aside and leaves whatever it pointed at alone, so a skill
managed by another tool keeps its own store.

Automatic syncs never replace anything.

Returns one outcome per skill per root rather than stopping at the first
conflict.

```json
{
  "ok": true,
  "outcomes": [
    { "root": "claude-user", "directory": "review", "status": "created", "message": null }
  ]
}
```

`status` is `created`, `updated`, `unchanged`, `removed`, `conflict`, or
`error`.

A propagated copy carries an `.aoe-managed.json` marker naming its root, its
skill, and the package digest at the time it was written. That marker is the
only thing that lets AoE later replace or remove the directory, and only while
the copy still matches the recorded digest. So a skill you wrote by hand, or a
propagated copy you have since edited, is reported as a `conflict` and left
exactly as it is; it is never overwritten, and it is never removed when its
managed source is deleted. A copy carrying a valid marker is listed once, as its
managed original, rather than twice.

This is also what makes AoE safe to run alongside a symlink-based skill manager
such as `skillshare`: a symlinked skill directory is something AoE did not
deploy, so it is reported and left in place rather than followed or replaced.

Setting `skills.auto_propagate` runs the same sync at session launch for the
agent being launched. It is off by default because it writes into your real
agent config directories.

All skill mutations require a read-write server and an elevated authenticated
session when login is enabled. They are unavailable in CityHall mode. Adoption
rejects symlinks, special files, packages over 64 MiB, individual files over
32 MiB, more than 1,024 files, and directory nesting deeper than 16 levels.

## GET /api/sessions

List sessions. Returns every session by default, including trashed and
archived ones; pass `state` to filter server-side instead of fetching
everything and filtering client-side.

**Query parameters**

| Name | Default | Notes |
| --- | --- | --- |
| `state` | (unfiltered) | `live` excludes trashed and archived sessions. `trashed` returns only trashed sessions. `all` (or omitting the param) is the historical unfiltered behavior. An unrecognized value is rejected with `400` rather than ignored, so a typo surfaces instead of silently returning every session. |

Each session row includes `context_resume`, the request-invariant availability
of preserving that agent's context across a later lifecycle transition. It is
an object tagged by `state`, with a `reason` on every state but `available`.

| `state` | `reason` values | Meaning |
| --- | --- | --- |
| `available` | (none) | A resume target exists and the launch path will use it. |
| `indeterminate` | `runtime_check_required`, `agent_handshake_required` | The answer needs a runtime probe this endpoint does not perform. Treat it as "ask again at launch", not as a no. |
| `unavailable` | `agent_unsupported`, `sandbox_unsupported`, `command_unsupported`, `forced_fresh`, `invalid_target`, `fork_pending`, `previous_failure`, `no_target` | Context will not be preserved. |

A daemon older than this field omits it entirely. Treat an absent
`context_resume` as unreported rather than as `unavailable`.

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

Successful creation returns `201`; an idempotent replay returns `200`. Both
include `aoe-runtime-epoch` and `aoe-runtime-revision`, and the response row
comes from that published snapshot. The session and its group hierarchy are
committed before the receipt. A native client must apply a snapshot from the
same epoch at or beyond that revision before acting on the new row.

An optional `size` object, such as `{"cols":137,"rows":41}`, sets the initial
terminal dimensions. Both dimensions must be nonzero. Replaying a creation
does not relaunch or resize its existing pane.

Disconnecting after admission does not cancel creation. Retry with the same
`idempotency_key` to recover its result without creating another session.

While creation is active, daemon requests to rename or delete its profile
return `409`. Hooks can still mutate other profiles; no catalogue lock is
held while they run.

The daemon validates planned resource paths and publishes a reserved `Starting`
row before provisioning directories or Git worktrees. These references protect
borrowed repositories and directories during Git hooks and `on_create`. Failed
provisioning or hooks roll back only the matching creation generation, using
shared-resource guards and the recorded ownership of checkouts and branches.
Cleanup failures retain resources.
Repository hook and MCP approvals are persisted against the source repository
before provisioning begins. Failure to persist approval aborts creation.

For `trust_hooks`, omission refuses unapproved repository hooks with
`hooks_need_trust`; `true` approves hooks and project MCP; `false` explicitly
skips untrusted hooks and MCP. Skipping retains global/profile hooks and any
already-trusted repository surfaces without granting or revoking approval.
Plugin creation always refuses unapproved repository hooks.

`POST /api/sessions/creation-trust` reviews `{path, profile?, scratch?}` without
granting trust or provisioning a session. It returns merged and repository-only
hooks, MCP summaries with environment/header names but not their values,
`hooks_need_trust`, `mcp_need_trust`, and a `fingerprint`. Unknown profiles are
rejected without creating them; read-only and CityHall modes refuse this route.

Pass the returned fingerprint as `trust_review` with the chosen `trust_hooks`
decision in the creation request. A changed source repository, global/profile
hooks, repository hooks, or project MCP produces `409` with
`aoe-error-code: creation_trust_changed`, before approval is persisted or
resources are provisioned. Review again rather than approving unseen changes.
Discarding a review requires no cancellation or cleanup.

`POST /api/sessions/{id}/creation/cancel` requests deferred cancellation while
provisioning or creation/launch hooks are pending. `202 Accepted` carries the
runtime epoch and revision headers like every other mutation, so a native caller
can fence the acknowledgement against the snapshot it was applied at. It is not
a canonical commit receipt: the current phase finishes before the daemon rolls
back its owned resources. The original creation request then returns `409`
with `aoe-error-code: creation_cancelled`. Cleanup failures retain resources.
Repeated requests remain accepted until finalization; after the hook boundary
or completion, the route returns `409` with `creation_not_pending`. Cancelling
does not revoke trust or undo arbitrary hook effects. A disconnected client
does not implicitly cancel admitted work. Read-only mode refuses cancellation;
CityHall permits only structured targets. Wait for the authoritative snapshot
before treating the row as removed.

While a creation is in flight the daemon reports its phase, running hook
command and bounded output tail on the runtime stream
(`/api/runtime/ws`) as a `creation` frame. It is advisory display state, never
canonical state: the row's own status and the cancel route remain the only
sources of truth, and an empty list means no creation is in flight. The frame
goes only to the stream the daemon authorized as the local owner, because hook
output is the documented channel for session environment secrets; a loopback
bearer client and a remote client receive canonical snapshots alone.

**Query parameters**

| Name | Notes |
| --- | --- |
| `wait` | Set to `ready` to block the response until the new session's status leaves `Starting` (or a 10s bound elapses), instead of returning immediately while the agent process is still coming up. The response `status` field reflects whatever the session actually reached, including `Error` if startup failed; a timeout does not mean success. |

**Conversation source**

`fork_session_id` names an existing AoE session row. The daemon resolves its
provider conversation ID; the request must use the same tool, view and resolved
ACP agent. This field is mutually exclusive with `fork_from` (a provider ID)
and `import_acp_session_id`. A missing row returns `404`, an active lifecycle
reservation returns `409`, and a missing conversation or incompatible provider
returns `400`. CityHall mode refuses canonical forks with `403`. An idempotent
replay still succeeds when the parent row has since been removed.

**Worktree fields**

| Field | Notes |
| --- | --- |
| `worktree_enabled` | Set `true` to create a managed git worktree even when no explicit branch name is supplied. |
| `worktree_branch` | Optional explicit branch or worktree name. If omitted while `worktree_enabled` is true, AoE derives a safe branch name from the resolved session title. |
| `create_new_branch` | `true` creates a new branch; `false` attaches to an existing branch. |

For compatibility, callers that only send `worktree_branch` still opt into
worktree mode. To get title-derived branch names, send `worktree_enabled` as
`true` and omit `worktree_branch`.

Workspace creation validates every repository before creating its directory.
An existing workspace target is refused rather than adopted or removed during
rollback.

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

## POST /api/sessions/{id}/restart

Explicitly relaunch a session, including a running agent. Unlike `/start`, which
leaves an already-running session alone, `/restart` performs the restart lifecycle.
Optional `profile`, `tool`, `command_override`, and `extra_args` replace the
authoritative launch settings within the daemon's admitted operation. Omitted
settings remain unchanged; a refused operation does not persist these edits.

```json
{ "command_override": "my-agent", "extra_args": "--verbose", "unsnooze": true }
```

The response includes the session and an `outcome` containing
`lifecycle_generation`, `profile`, and the prepared terminal `target` (`null` for
structured sessions). The receipt cursor is carried in `aoe-runtime-epoch` and
`aoe-runtime-revision` response headers, not in the JSON body. Native clients
attach only after applying a snapshot at or beyond that receipt in the same
epoch, with matching lifecycle generation, profile, and live terminal identity.

## Agent hook acknowledgement

A host (non-sandboxed) session whose agent installs AoE status hooks will not
launch until the daemon's machine has acknowledged what that install writes.
Until then `POST /api/sessions` answers `400` with
`aoe-error-code: agent_hooks_not_acknowledged`.

`GET /api/app-state/agent-hooks-acknowledgement?tool=&profile=` reports it for
one agent. Both query parameters are optional and default to what a create with
no explicit choice would use. The response names the resolved `agent`, whether
the acknowledgement is `required` and already `acknowledged`, and a
`disclosure` of the settings files the install writes, the hook events it adds,
and the command each hook runs.

`POST /api/app-state/agent-hooks-acknowledgement` records the acknowledgement.
It is refused in read-only and CityHall modes: consenting writes into the
daemon user's own agent settings.

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

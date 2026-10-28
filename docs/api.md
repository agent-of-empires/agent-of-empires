# HTTP API Reference

`aoe serve` exposes a small HTTP API so external orchestrators (other
agents, MCP tools, CI scripts) can drive sessions without attaching to
a terminal. This page documents the orchestration endpoints. The web
dashboard uses the same API surface plus additional internal routes.

## Authentication

TCP endpoints require authentication unless the server was started with
`--no-auth`; see [dashboard security](guides/web-dashboard.md#security) for
token and passphrase modes. Token authentication accepts three transports:

| Transport | Example |
| --- | --- |
| Bearer header (recommended for clients) | `Authorization: Bearer <token>` |
| Query parameter | `?token=<token>` |
| Cookie | `aoe_token=<token>` (set automatically by the dashboard) |

The private local Unix socket verifies its owner instead. A native client using
that socket does not need the TCP passphrase or a loopback login, including when
the dashboard runs behind a proxy.

Local native transport trusts processes running as the same OS user. Kernel peer
credentials establish UID ownership, not the identity of a particular daemon
instance or executable. Namespace, tmux locator, and protocol checks reject
accidental runtime mismatches; they do not authenticate a malicious same-UID peer.

Native clients require HTTPS for credentials outside loopback and disable
redirects. Local native connections bypass environment proxies. Their error
diagnostics omit login and elevation response bodies, which can reflect secrets.
Native HTTP operation deadlines cover connection, headers, and the complete body
stream with one absolute budget, including explicit longer operation timeouts.

Read-only mode (`aoe serve --read-only`) blocks every write endpoint
with `403 read_only`. Read endpoints work normally.

## MCP server

Set `[web] mcp_enabled = true` in global `config.toml`, then restart the
Daemon to enable Streamable HTTP at `/mcp`. The route is absent by default.
It shares the existing API router and does not expose TCP or web assets when
running core-only. With token authentication, `/mcp` requires a valid token,
sent as an `Authorization: Bearer` header (the form `aoe mcp config`
generates), an `aoe_token` cookie, or a `?token=` query parameter, and returns
`401` when it is missing or invalid. With passphrase authentication, a request
needs a valid `aoe_session` login cookie, accepts no bearer token, and gets
`401 login_required` without one. The login wall covers loopback callers as
well as remote ones. A token-authenticated daemon that also has a passphrase,
such as `--remote`, still requires that login session on `/mcp`, so a bearer
token alone is not enough. A daemon running `--auth none` needs no credential.
The private `daemon/api.sock` accepts the authenticated same-UID owner peer
without a token. To connect an agent, see
[Controlling AoE over MCP](guides/mcp-control.md).

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_sessions` | `state` (`live`, default, or `all`), `group` (exact path and descendants), `children_of` (direct parent id) | The `{sessions, workspace_ordering}` envelope from the published canonical snapshot. |
| `whoami` | none | `null`; no session credential is currently available. |

Both tools declare an `outputSchema` and return `structuredContent`.
Listing does not persist workspace ordering or rebuild rows from mutable state.
Read-only mode permits these reads. CityHall rejects `POST /mcp` with `403`,
as it does other non-allowlisted POST routes, including over the owner socket.
MCP POST requests remain tracked runtime work: shutdown interrupts them, and a
long-running request can delay draining. No mutation or automatic replay is added.

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

Native clients classify errors from the status and one `AoE-Error-Code` response header, never from the body. A missing, repeated, unknown or status-inconsistent value means "generic status". Plain HTTP from a non-loopback peer gets `426 tls_required` before authentication unless the bind sets `--dangerously-allow-plaintext-remote`.

## Runtime CLI reads

The covered CLI reads (`list`, `status`, `session show`, `session list-trash`, `group list`, `profile list` or bare `profile`, and `project list`) require a Daemon answer. They use `/api/runtime/read/ws`, over the Daemon's local Unix socket by default and over HTTP when `--daemon-url` or `AOE_DAEMON_URL` selects an endpoint. `/api/runtime/ws` remains the separate TUI stream.

The covered local command handlers return an internal error if runtime dispatch is bypassed; they never read storage.

Explicit and environment-selected endpoints never fall back to local storage or start a local Daemon. Invalid configuration, connection failures, authentication errors, and protocol errors are final refusals against the selected target. Unreachable targets leave stdout empty and report reconnect guidance on stderr. Environment URLs retain a one-second TCP establishment budget; each served exchange has a 15-second deadline.

Covered reads still run the monolithic CLI's local startup preflight, including schema migrations, before contacting the Daemon. A local schema or migration failure can therefore block a read even with an explicit or environment-selected remote URL. Selecting an endpoint does not bypass this preflight.

CLI commands that still write storage directly, such as `session archive`, `session snooze` and `rm`, can finish before the Daemon adopts their changes. Covered reads are eventually consistent with those writes, including when the Daemon was already running: a successful write does not guarantee immediate visibility in the next `list`, `session show` or `session list-trash`. Automation must wait for the expected state rather than assume read-after-write consistency; no fixed adoption deadline is guaranteed. The remaining writer cutover is tracked by [#4467](https://github.com/agent-of-empires/agent-of-empires/issues/4467), following [ADR 0004](adr/0004-cli-writes-through-the-daemon.md).

After upgrading the CLI, a `protocol_mismatch` means the target daemon uses a different protocol version. On that daemon's host, run `aoe serve --restart` for a detached daemon; otherwise restart the foreground daemon or its service through the service manager. Update that host's installation if needed. A missing runtime-read route can also indicate an older daemon, but first check the selected URL and proxy routing.

Without an HTTP endpoint, the CLI acquires the local Daemon through the [shared local auto-start policy](adr/0004-cli-writes-through-the-daemon.md), which starts one only when the API Unix socket is proven absent, then reads through that socket with peer-UID verification on Linux and macOS. A Client without a local backend requires a remote URL.

An implicit local read can still refuse after acquisition if the Daemon does not serve runtime reads. Check its mode and version compatibility; CityHall intentionally disables these reads on both transports.

A `403` carrying a valid `AoE-Error-Code: cityhall_mode` produces the CLI refusal code `cityhall_mode`, not `unauthorized`. This is a deployment-mode restriction; retrying authentication does not enable runtime reads. The refusal does not trigger login retries, a different target or local startup.

Identity refusals such as `peer_identity` require checking the expected Daemon, app/build namespace, versions and socket ownership through its manager. Do not start another Daemon while ownership is uncertain.

`--no-autostart` or `AOE_NO_AUTOSTART=1` disables local startup, not reuse of an existing Daemon. Startup failure or an absent Daemon with startup disabled is an error with guidance, never a disk answer. The spawning Client announces `Started aoe daemon (pid N)` only on stderr, including under `--json`. Local administrative commands keep their existing startup and configuration-recovery behavior; other reads outside this covered set remain unchanged.

The startup profile does not determine read scope. Reusing an existing Daemon does not resolve or validate a startup profile. A missing configured default does not block an explicitly scoped read or a profile-independent read; an unscoped read still reports that missing default from the Daemon snapshot. Only automatic read acquisition preserves a configured missing default in an empty profile registry. Administrative startup and native acquisition retain their initial profile creation. Configured aliases retain their read scope without becoming physical startup profiles.

On a genuine first run with no configured default, an unscoped read retains the existing `main` bootstrap during local preflight, even with auto-start disabled.

The route follows the daemon's authentication mode. CLI clients use `AOE_DAEMON_TOKEN` or `AOE_DAEMON_PASSPHRASE`; passphrase login shares the ACP client's device-bound session cache. Environment URLs discard query parameters, including query tokens. Explicit endpoints reject queries. HTTP on a non-loopback host is accepted only without credentials; remote authentication requires HTTPS. Localhost connections bypass proxies and pin both loopback address families.

Loopback HTTP does not verify the listener's OS user. If another local user binds the selected port, that process can receive the CLI's bearer token or passphrase. Use it only with trusted local users and port ownership; unset the HTTP endpoint to use the verified same-user Unix socket.

Runtime snapshots include stored launch commands on both transports to preserve CLI output. Commands may contain inline secrets; treat clients allowed to read snapshots as trusted, and do not expose `--auth none` to untrusted peers. Human output from served reads strips C0/C1 control characters from daemon-provided fields; JSON retains the original values.

Wire protocol **version 4** sends a `Hello` followed by a `Snapshot`. Profiles carry `name`, `listed` and `aliases`; aliases select the same physical store while preserving the requested spelling. Alias-only external stores are available through scoped reads but excluded from ordinary profile inventories and all-profile lists.

Eligible verified-local CLI JSON listings include the display-only scope glob
`aoe-session-<escaped-id>-*.scope` when local systemd scopes are enabled and
visible. Remote listings omit it. This pattern matches legacy and namespaced
unit names and may span application namespaces for the same session id. It
proves neither unit existence nor ownership. The Daemon retires only captured
exact units; the frozen protocol-4 snapshot does not carry scope metadata.

Runtime-read session identity and parent relationships are scoped to a profile. The same session ID may appear in different profiles without invalidating the snapshot. Stored absolute project paths retain their spelling, including legacy `.` / `..` components and repeated separators; reads do not normalize them or resolve them on the client.

Filesystem work that follows profile aliases runs in isolated workers. Each alias operation waits at most 250 ms for a fresh result; a blocked alias degrades only its own data. Several blocked aliases or operations can accumulate these waits. Cached session rows retain their physical-store identity and cannot be relabelled as a replacement store. Stalled local profile or application-directory operations can still block refresh and mutations waiting on the reload gate. Client deadlines do not cancel filesystem syscalls.

Scoped session reads refuse whole-profile registry decode failures, such as malformed JSON or a non-array container, rather than returning an empty success. The legacy read-only projection skips individually undecodable rows without rewriting the registry; a registry containing only a row missing its `id` can therefore produce `[]` with exit status 0 for a JSON session list. Successfully decoded rows that violate the runtime-read wire contract degrade that profile's session data.

All-profile listing preserves healthy profiles' rows and warns on stderr for each unavailable profile. Human output exits successfully; JSON remains valid but exits nonzero when the listing is incomplete. Invalid stored groups degrade only that profile's group enumeration; invalid global projects degrade only global metadata. These components remain independent.

A failed profile enumeration retains accepted rows and runtime-only overlays.
Repaired files alone do not restore accepted profile-data health; a reload must
adopt the recovery. Native health clears only after complete canonical adoption,
not a partially successful load.

The CLI projection is not native mutation authority. Complete canonical profile loading can still degrade native health and refuse native mutations with HTTP 503 while healthy CLI reads continue. Snapshot sampling never rewrites the stores.

Newly listed profile stores are read on a cache miss without replacing existing live session rows. A newly listed store reusing a cached profile name or alias is not admitted as a cache miss; the daemon must accept a reload first. This includes deleting and recreating a profile with the same name, preventing retained live rows from being mixed with replacement-store data.

Wire members declared nullable must still be present. CLI JSON is a separate contract; for example, `session show --json` omits an unknown `agent_session_id`. A `systemd_unit` glob in local list JSON describes an expected scope name, not proof that the unit exists; remote snapshots omit it.

Background daemon maintenance can recover missing native session IDs from validated publications after the agent pane exits. Read requests do not perform these writes.

## GET /api/sessions

List sessions. Returns every session by default, including trashed and
archived ones; pass `state` to filter server-side instead of fetching
everything and filtering client-side.

The response is an object, not a bare array:

```json
{
  "sessions": [ { "id": "…", "status": "Running" } ],
  "workspace_ordering": ["/path/to/repo::main"]
}
```

- `sessions`: the session rows, in the daemon's canonical order.
- `workspace_ordering`: the user's persisted manual workspace order, so a
  client can render workspaces in place without a second request. Unknown
  workspaces (present in `sessions`, absent from this list) follow the
  list, newest first. A client that never issues
  `PUT /api/workspace-ordering` always receives an empty array.

Clients written against the historical bare-array response must be updated to
read `sessions`; the array is never returned unwrapped.

`aoe-runtime-epoch` and `aoe-runtime-revision` identify the exact returned
snapshot. Revisions are decimal unsigned 64-bit integers; clients must preserve
their precision. A stale list must not replace newer canonical rows or ordering.
Color and notification mutation receipts identify one session: another session
at a higher revision cannot satisfy that receipt.

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

An omitted `profile` targets the served profile reported by `GET /api/about`, not
the machine-default profile. Load project registries and launch defaults for that
same profile until an explicit profile is selected.
If `/api/about` is unavailable, keep creation disabled until the served profile
is resolved rather than guessing a default. The web wizard provides **Retry
server profile** and preserves choices already edited by the user.

An optional `size` object, such as `{"cols":137,"rows":41}`, sets the initial
terminal dimensions. Both dimensions must be nonzero. Replaying a creation
does not relaunch or resize its existing pane.

Disconnecting after admission does not cancel creation. Retry with the same
`idempotency_key` to recover its result without creating another session.

While creation is active, daemon requests to rename or delete its profile
return `409`. Hooks can still mutate other profiles; no catalogue lock is
held while they run.

The daemon validates planned resource paths and publishes a reserved `Creating`
row before provisioning directories or Git worktrees. These references protect
borrowed repositories and directories during Git hooks and `on_create`. Failed
provisioning or hooks roll back only the matching creation generation, using
shared-resource guards and the recorded ownership of checkouts and branches.
Cleanup failures retain resources.
Structured rows cannot resume, receive prompts, or replay an unfinished creation
while these hooks are pending. Matching plugin retries wait for finalization.
An orphaned HTTP retry receives `409 create_outcome_unknown`; concurrent keyed
HTTP requests wait for the admitted creation to finish.
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

On quit, the TUI gives its original authenticated daemon up to two seconds to
resolve that creation's exact request key and acknowledge direct cancellation.
This lane bypasses queued commands but never reconnects, changes epoch, or
replays the cancellation. An acknowledgement still means admission, not
completed rollback; an unconfirmed outcome is logged before the TUI exits.

While a creation is in flight the daemon reports its phase, running hook
command and bounded output tail on the runtime stream
(`/api/runtime/ws`) as a `creation` frame. It is advisory display state, never
canonical state: the row's own status and the cancel route remain the only
sources of truth, and an empty list means no creation is in flight. The frame
goes only to the stream the daemon authorized as the local owner, because hook
output is the documented channel for session environment secrets; a loopback
bearer client and a remote client receive canonical snapshots alone.

`GET /api/sessions/create-progress/{key}` also exposes bounded hook progress.
Browser-created progress remains readable under ordinary HTTP authentication.
Progress created through the owner Unix socket returns `404` to TCP clients,
including loopback bearer clients; the Unix owner still receives it.
This visibility follows creation origin, not secrecy of the public correlation key.

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

## POST /api/sessions/{id}/restart

Explicitly relaunch a session, including a running agent. Unlike `/start`, which
leaves an already-running session alone, `/restart` performs the restart lifecycle.
Optional `profile`, `tool`, `command_override`, and `extra_args` replace the
authoritative launch settings within the daemon's admitted operation. Omitted
settings remain unchanged; a refused operation does not persist these edits.
Structured sessions replace their ACP worker without creating a tmux session.
The outgoing worker must exit before its replacement is admitted.

```json
{ "command_override": "my-agent", "extra_args": "--verbose", "unsnooze": true }
```

The response includes the session and an `outcome` containing
`lifecycle_generation`, `profile`, and the prepared terminal `target` (`null` for
structured sessions). The receipt cursor is carried in `aoe-runtime-epoch` and
`aoe-runtime-revision` response headers, not in the JSON body. Native clients
attach only after applying a snapshot at or beyond that receipt in the same
epoch, with matching lifecycle generation, profile, and live terminal identity.

## POST /api/sessions/{id}/send

Type a message into the agent and press Enter, the same way the TUI's
send-message dialog and the `aoe send` CLI do. Honors the per-agent
paste-burst delay (e.g. Codex needs ~150 ms between text and Enter so
its burst-detection window expires before Enter arrives).

**Request body** (JSON)

```json
{ "message": "review the diff and pick the smallest fix" }
```

Before delivery, the HTTP endpoint removes bracketed-paste delimiters and remaining ESC bytes, then removes only trailing CR/LF characters. Internal line breaks, spaces, and tabs remain. The terminal transport adapts pasted LF to CR and adds a final Enter. An empty or whitespace-only result is rejected.

**Responses**

| Status | Body | When |
| --- | --- | --- |
| `200` | `{"sent": true}` | Keys delivered to the tmux pane |
| `400` | `{"error": "message_empty"}` | `message` is empty or whitespace-only |
| `400` | `{"error": "acp_mode_unsupported"}` | Session is structured-view/ACP mode and has no tmux pane |
| `403` | `{"error": "read_only"}` | Server is in read-only mode |
| `404` | `{"error": "not_found"}` | No session with that id |
| `409` | `{"error": "session_not_running"}` | Session exists but the tmux pane is gone |
| `409` | `{"error": "resume_failed", "message": "...", "resume_session_id": "..."}` | Auto-revive tried to resume a stored conversation, but the pane exited during the probe. The ID and failure marker are preserved in this call; the next default revival can start fresh. |
| `409` | `{"error": "session_transient", "status": "..."}` | Session is mid-lifecycle and cannot accept input yet |
| `409` | `{"error": "session_archived" \| "session_trashed", "message": "..."}` | Session is archived or trashed, even with a live pane or `revive: false`; unarchive or restore it first. `/start`, `/ensure`, `/container-terminal` and structured-view prompts refuse the same way. |
| `500` | `{"error": "tmux_error"}` or `{"error": "internal"}` | Unexpected failure (logged server-side) |

After a prior failed resume probe, a default restart or send-driven revival can start a new conversation and deliver input there. The old native transcript is not deleted, but its AoE association is abandoned. `/ensure` reports this as `resume_outcome: "fresh_after_failed_resume"` with `prior_session_id` and a message, rather than claiming a successful resume. See [session recovery policy](guides/session-resume.md#pinning-or-resetting-a-conversation) for explicit retry and direct CLI start behavior.

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

## Native session freshness and disconnection

The Daemon owns native session status. The experimental TUI consumes its
published snapshots and does not run a second local status poller.
Status sampling runs every 500 ms while a native stream has subscribers,
and every two seconds otherwise. Under healthy local filesystem and tmux
conditions, the steady-state target is an observed status change within
two seconds in the TUI. This target excludes startup grace and does not
bound stalled filesystem or tmux operations.

For a committed launch, the Daemon retains `Starting` for up to three
seconds from persisted pane creation time. Active lifecycle reservations
retain their separate ownership timeout. Missing or future-dated pane
births do not extend presentation grace.

After connection loss, the TUI hides session rows and refuses local
mutation or input fallback until explicit reconnection supplies a fresh
snapshot. Retained state serves reconciliation fences, not stale display.
An independent native tmux client attached before connection loss remains
attached; stopping the Daemon does not revoke that client's tmux access.

## Native mutation completion

Each receipt belongs to its original HTTP request. The TUI routes that response
through the command's dedicated oneshot channel and permits only one pending
mutation per session row; concurrent creations retain separate caller tokens.
The epoch/revision cursor is a snapshot-application fence, not a request identifier.
A snapshot alone cannot settle a command whose HTTP response has not arrived.

The TUI settles a mutation only after applying a same-epoch snapshot at
or beyond its receipt. If profile storage cannot be loaded, the TUI retains the
unapplied snapshot and creation receipt and retries the load without resubmitting
the mutation or rerunning creation hooks.

Cancelling an attachment or input continuation does not revoke daemon authority.
A confirmed mutation still settles behind its receipt, but no cancelled input or
attachment is delivered. Genuine authority loss or an unknown outcome remains
blocked until reconciled.

A failed launch can still commit a lifecycle generation. The finite
`aoe-error-code` values `start_failed`, `resume_failed`, and
`managed_resume_refused`, with valid same-epoch runtime receipt headers,
acknowledge that preparation commit, not a successful
launch. Clients must apply its snapshot before retrying. A later send or target
probe failure can likewise retain a preparation cursor; it does not acknowledge
message delivery. Transport loss or malformed mutation receipts remain unknown;
effect-free daemon refusals do not acknowledge a commit.

`409 managed_resume_refused` preserves the managed conversation. Its finite
`aoe-managed-resume-cause` header supplies an application-owned recovery reason.
Missing, unknown, or ambiguous cause metadata keeps the refusal generic; native
clients do not read response bodies to derive that reason. See
[managed resume recovery](guides/session-resume.md#common-managed-resume-and-fork-refusals).

A failed purge can commit a retained-row failure marker. `409 purge_failed`
includes the finite code header and its canonical receipt; clients wait for that
snapshot before retrying or escalating. Unlike an unknown outcome, this committed
cleanup failure does not require an operator to resolve mutation authority.

With an active local daemon, CLI profile renames and deletions go through its
namespace epoch. Deleting its served profile returns `409 served_profile` and
preserves the profile; renaming it retargets the live identity. A daemon refusal
never falls back to editing the profile directory directly.
Retained purge owners also refuse rename or delete with `409 pending_purge`.
This includes supported `sessions.json` or `groups.json` file aliases sharing
the retained owner's physical registry, even when the owner uses another profile name.
Complete purge recovery before retrying; this busy refusal does not degrade runtime health.

Purge refuses unresolved runner ownership before committing deletion. The row
and its resources remain available for repair and retry.

Delete, purge and restore share one Busy contract. Busy means another lifecycle
operation holds the session, nothing changed, and the caller may retry once it
finishes. It is not `KeptRestored` (restored mid-purge), already gone, or a
cleanup failure. Over HTTP it is `409` with the `lifecycle_locked` code header
and `{"error":"lifecycle_busy","operation":"<holder>"}`; a selection that
changed after it was made is `409` with its own `lifecycle_superseded` code
header; it may already be partly processed, so refresh and check before retrying. A batch delete that skips Busy sessions lists them under `busy` and
reports `partial`; when nothing else completed it answers `409 lifecycle_busy`.
`aoe rm --purge` and `aoe session restore` exit non-zero for Busy. `aoe session
empty-trash` purges the rest, lists each skipped session on stderr, then exits
non-zero.

Selected-trash actions include `expected_trashed_at`, `expected_generation` and
`expected_trash_lifecycle_id`. The durable identity distinguishes restore/retrash
even when timestamps coincide.
`force_failed` permits escalation only after a recorded failure for that exact
selection. `drop_failed` removes the row only after a forced failure for the same
selection; it retains worktree files. Restore or a new lifecycle generation
invalidates the failure marker.

## Explicit structured worker stop

`DELETE /api/sessions/:id/acp` is a durable user stop, not a daemon detach. It commits the native stopped/dormant generation before retiring the captured original worker and returns `204` only after retirement is proven. Generic `POST /api/sessions/:id/stop` uses the same structured-worker stop boundary.

Admitted stop work survives HTTP cancellation; its committed native fence survives daemon reconstruction. Automatic reconciliation does not resume a committed stop. A later explicit spawn, start, restart, backend/provider switch or accepted prompt proves original retirement before consuming only its observed stop generation and token; a newer stop wins. A `no_revive` prompt cannot clear the stop.

If a write commits but canonical publication fails, the stop remains durable and the response reports an unconfirmed outcome. Unproven retirement remains pending rather than reporting success. Daemon detach, idle shutdown and internal provider teardown do not independently create user-stop intent. Reversible stop preserves native transcript history.

## DELETE /api/workspaces

Permanently purge an explicit `session_ids` selection. Capture the IDs and trash consent when opening confirmation; do not rebuild the selection from a later list. Duplicate IDs are processed once. The server identifies the managed worktree owner and removes selected siblings before it.

For a trash-only selection, send `expected_trash: true` and one original consent per selected ID:

```json
{
  "session_ids": ["example-session"],
  "expected_trash": true,
  "trash_consents": {
    "example-session": {
      "expected_trashed_at": "2026-10-11T00:00:00Z",
      "expected_trash_lifecycle_id": "11111111-1111-4111-8111-111111111111",
      "expected_generation": 7
    }
  }
}
```

Use values from the confirmed rows, not the illustrative values above. Consent keys must belong to `session_ids`; a trash-only request must cover every selected ID. Malformed UUIDs, extra keys or incomplete coverage return `400 invalid_trash_consent` before reservation or teardown. Restore, retrash or a changed generation invalidates that row's original consent.

For mixed live/trash selections, omit `expected_trash` or set it to `false`, and include consent for each selected trashed row. A consent still makes that row trash-specific; it does not authorize deleting a restored row.

Optional cleanup flags are `delete_worktree`, `delete_branch`, `delete_sandbox`, `force_delete` and `keep_scratch`, all defaulting to `false`. Worktree and branch cleanup apply to the selected owner; resources still referenced by unselected sessions remain protected. A dirty shared worktree returns `409 dirty_worktree` unless forced.

A successful HTTP response reports `status` (`deleted`, `kept` or `partial`), `deleted` and `kept` ID arrays, `failed` entries containing `id` and `error`, and `messages`. Processing can stop at the first retained or failed row, leaving later IDs unprocessed and absent from these arrays. A response with no confirmed deletion and a failure returns `500 deletion_failed`; publication uncertainty can return `503`.

Only IDs explicitly reported in `deleted` authorize removing local drafts, cached settings, selection or navigation state. Preserve `kept`, failed and omitted IDs. A 2xx response alone does not confirm the entire selection; a missing `deleted` array or transport failure requires reconciliation, not a replay of the old confirmation.


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

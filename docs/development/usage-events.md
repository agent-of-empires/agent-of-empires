# Usage events

AoE records agent context resets, prompts, turns, and session lifecycle events
in `usage.db` in the app directory. This page is the contract for external
readers such as Harness Ledger.

## Reading

Prefer `aoe usage export --after-id N`: one JSON object per line, oldest
first, with the fields below. Remember the last `id` you imported and pass it
back as `--after-id`. Readers may also open the database read-only; writers
use WAL mode.

## Table `usage_events`

| column | type | notes |
|---|---|---|
| `id` | integer | monotonic; the import cursor |
| `occurred_at` | text | RFC 3339 UTC with milliseconds |
| `instance_id` | text | the AoE session id |
| `profile` | text, nullable | AoE profile |
| `agent` | text, nullable | `claude`, `codex`, `pi`, or the session's tool |
| `kind` | text | see below |
| `detail` | text, nullable | at most 32 chars of `[a-z0-9_]` |
| `agent_session_id` | text, nullable | the agent's native session id |

`usage_meta.schema_version` is `1`. Within a schema version, changes are
additive only: new kinds, new detail values, new nullable columns.

## Kinds

| kind | detail | source |
|---|---|---|
| `context_start` | `startup` `resume` `clear` `compact` `fork` `reload` | Claude and Codex `SessionStart.source`; Pi `session_start.reason` (`new` is stored as `clear`) |
| `context_end` | agent reason, verbatim | Claude `SessionEnd.reason`; Pi `session_shutdown.reason` |
| `compact` | `manual` `auto` | Claude and Codex `PostCompact.trigger`; Pi `session_compact.reason` (`threshold` and `overflow` are `auto`) |
| `prompt` | none | Claude and Codex `UserPromptSubmit`; Pi interactive or RPC `input` |
| `turn_end` | none, or `error` | Claude and Codex `Stop`; Claude `StopFailure` (`error`); Pi `agent_settled` |
| `instance_created` | none | AoE created the session |
| `instance_restarted` | none | AoE restarted the session's agent |
| `instance_deleted` | none | AoE deleted the session |

A compaction logs both `compact` and `context_start` with detail `compact`.
Count compactions and context boundaries from `compact` only.
A `context_start` with detail `reload` (Pi reloading the same conversation) is not a new context.

Sandboxed sessions record lifecycle events only. Sessions launched through a
launcher that renders its own agent settings (for example Harness Ledger
profiles) record agent events only when that launcher's settings include
AoE's `__usage-event` hook command; otherwise they record lifecycle events
only.

Claude and Codex retain usage reporting alongside their identity hooks when status reporting is disabled. Codex's native hooks feature must remain enabled.

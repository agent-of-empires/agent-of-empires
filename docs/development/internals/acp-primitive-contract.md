# ACP primitive contract (`_meta.aoe`)

Core consumes generic `_meta.aoe` primitives plus standard ACP. Anything specific to one agent's own `_meta` is translated into these primitives outside core, usually by a plugin-shipped proxy sitting between the worker and the adapter. The wire shape is published as [`assets/aoe-meta-v1.schema.json`](../../../assets/aoe-meta-v1.schema.json), generated from `src/acp/aoe_meta/`.

## Three versions

Each moves independently.

| Version | Where | Meaning |
|---|---|---|
| Manifest `api_version` | plugin manifest | What the plugin may declare. |
| Proxy protocol version | `_meta.aoe.proxy.protocol_version` | The proxy process and hello contract. Core speaks `1`. |
| Primitive schema version | `_meta.aoe.v`, `_meta.aoe.proxy.primitive_schema_version` | The shape of the primitives below. Core speaks `1`. |

A frame or hello naming a version core does not speak is rejected.

## Hello

A proxy adds `_meta.aoe.proxy = {plugin_id, protocol_version, primitive_schema_version}` to the `initialize` response, next to the adapter's own unchanged `agent_info`. No hello means a plain ACP agent: it is not held to this contract and a malformed `_meta.aoe` from it is ignored. An invalid hello refuses the session with `StartupErrorDetail::InvalidProxyHello`.

## Envelope

`_meta.aoe` on a `session/update` or tool call:

```json
{"v": 1, "key": "call-17", "parent_tool_call": "call-3"}
```

- `key` is an idempotency key. A repeat is dropped, among the last 4096 keys of a native session; a reset starts a fresh key space. A proxy must not replay a key older than that window.
- `after_adapter_sequence: N` marks a frame the proxy emitted itself, for example from a transcript tailer. It is not an adapter frame and takes no number; core delivers it after adapter frame `N`. Adapter frames are numbered from 1 per connection and keep counting across a conversation reset. At most 1024 frames may wait; a reset drops them.
- `lease` is described below.
- Primitives: `parent_tool_call`, `ephemeral` (drop before the reducer), `memory_recall`, `plan_proposal`, `tool_presentation`, `option_preview`. `parent_tool_call`, `memory_recall` and `plan_proposal` are read on `tool_call` updates only, where they produce the same events as the matching agent-specific path. `tool_presentation` and `option_preview` are validated and carried; no surface renders them yet.

Frames are consumed in arrival order before they are mapped to events (`src/acp/acp_client/aoe_ingress.rs`).

A `session/update` with no `_meta.aoe` is an unanchored adapter frame: it is counted and delivered in arrival order, never held. A proxied agent may omit the envelope on any frame, which opts that frame out of `after_adapter_sequence` ordering and is not a contract violation.

## Leases

`lease: {id, kind: busy|task|compaction, op: acquire|renew|release, ttl_ms}` holds the session as working without further frames. `ttl_ms` is 1 to 3,600,000 and required for acquire and renew. A lease expires on its own, so a dead proxy drains. A live `busy` or `task` lease suppresses the silent-orphan and between-prompt watchdogs; any live lease refuses a conversation reset. Replayed history never grants a lease, and a conversation reset releases every lease.

## Failure

For a proxied agent, a frame that breaks the contract, an overfull hold buffer or an overfull lease table emits `Event::IntegrationLost` and core ignores the agent's later frames. A process that has not announced itself is never held to this.

## Conformance

`aoe acp lint-stream <ndjson|->` checks a recorded agent-to-client stream, one JSON-RPC frame per line, and exits 1 on any finding. `--require-hello` also fails a stream with no hello. Plugins run it in CI. It replays the stream through the same ordering as the runtime, starting a fresh key space whenever a result carries a new `sessionId`. An update for any other session than the latest result is skipped as the runtime drops it, but still takes its adapter number, so frames for a session that arrive before its `session/new` result are not checked. It checks the hello (on an initialize result only), envelope shape and version, empty keys, keys reused on a different update, lease ttl and unknown lease ids (applied at each frame's anchor), more than 1024 frames waiting on an anchor, anchors beyond the end of the stream, and the three `tool_call`-only primitives elsewhere. It does not count simultaneous leases: a recorded stream has no clock, so it cannot tell which have expired.

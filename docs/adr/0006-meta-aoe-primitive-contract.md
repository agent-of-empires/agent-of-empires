# Core owns generic `_meta.aoe` primitives; agent-specific meaning lives outside core

Status: accepted. Part of #5308. Terms follow `CONTEXT.md`: an **ACP adapter** speaks ACP to the **Worker**.

Core read each adapter's private `_meta` directly (`_meta.claudeCode.*`, `_claude/*`, tool titles), so every vendor schema change was a core change. Core instead defines a small versioned vocabulary under `_meta.aoe` and consumes only that plus standard ACP; whatever translates an adapter's own meta into it runs outside core.

## Decision

- The primitives carry their own schema version, independent of the plugin manifest `api_version` and the proxy protocol version. Three versions move separately.
- The translator is an inline, per-session proxy between the worker and the adapter, not a sidecar behind an RPC. One ordered stream needs one write lock and works the same in TUI-only mode and under `docker exec`.
- A proxy announces itself with a hello on the `initialize` response. Only a proxied agent is held to the contract; any other agent's malformed `_meta.aoe` is ignored.
- State primitives are self-healing: leases expire on their own, every primitive has an idempotency key, and tailer-emitted frames carry `after_adapter_sequence` so core orders them against adapter frames.
- Failure is closed: mid-session it emits `IntegrationLost`, and an invalid proxy hello at startup refuses the session (`InvalidProxyHello`).
- `aoe acp lint-stream` is the conformance check plugins run in CI. See [the contract](../development/internals/acp-primitive-contract.md).

## Considered options

- **A declarative mapping language in the manifest.** Rejected: a language core maintains forever that cannot express stateful work such as transcript tailing.
- **A sidecar over RPC.** Rejected: extra serialization on every `session/update`, new merge plumbing in the runner, and three streams to correlate.
- **Fail open mid-session.** Rejected: silently losing interpretation breaks visible UI such as nesting, plan approval and the usage banner.

## Consequences

- Adding a primitive is a schema version change with a documented policy; removing one is breaking.
- Agent-specific behavior can move out of core one primitive at a time.
- Direct filesystem access by a proxy is same-user trust, the same boundary as plugin report files. A narrow core filesystem service is the upgrade path if plugin sandboxing lands.

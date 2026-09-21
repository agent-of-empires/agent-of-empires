# Adding a New Agent

## Files touched

| File | Purpose |
|------|---------|
| `src/agents.rs` | Registry entry (name, binary, detection, flags) |
| `src/tmux/detect/manifests/<agent>.toml` | Detection rules, for a pane-parsed agent |
| `src/tmux/status_detection.rs` | Detection entry point (manifest call or stub) |
| `src/hooks/mod.rs` | Hook installer, if the agent supports hooks |
| `src/session/instance/hooks.rs` | Hook wiring plus the `AOE_INSTANCE_ID` env prefix |
| `src/session/config/container_config.rs` | Config mount for the sandbox |
| `src/acp/agent_registry.rs` | ACP adapter entry, if the agent ships an ACP server |
| `src/acp/agent_profiles.rs`, `web/src/lib/agentProfiles.ts` | Structured view profile |
| `src/acp/install_hints.rs` | Install hint for `aoe acp doctor` and handshake failures |
| `docker/Dockerfile` | Install the agent in the sandbox image |
| `docs/structured-view.md`, `docs/index.md`, `README.md` | Supported-agent lists |

## Levels of support

Each level is additive; do only what the agent supports.

| Level | What it gives | Requires |
|-------|---------------|----------|
| 1. Basic | Appears in `aoe agents`, sessions launch, status always Idle | `AgentDef` plus a stub `detect_status` |
| 2. Pane-parse status | Status inferred from terminal output | A manifest plus `detect_<agent>_status` calling into it |
| 3. Hook status | The agent writes status the instant it changes | `hook_config` with the generic `install_hooks()`, or `sidecar_hooks` with a custom installer |
| 4. Session resume | A restart resumes the same native conversation | A `session_support` contract with verified resume argv |
| 5. Sandbox | Runs isolated with host config synced in | `AgentConfigMount` plus a Dockerfile install |

Levels 3 and 4 are independent. `session_support` declares verified native resume argv; its optional capture spec is the only backend allowed to supply an id, with each environment explicitly `PaneScoped`, `Preassigned`, `ManagedExclusiveStore`, or `Unsupported`. Use `capture: None` for argv-only support, and omit `session_support` entirely when the resume argv itself is unverified. Managed-store capture additionally requires a physically per-instance store, a cwd match, a launch-time floor, and a cross-process ownership lease.

## Steps

**1. Research** the binary name, detection, YOLO flag, exact resume and fork argv, authoritative session-id source, host and sandbox storage paths, hook identity field, config dir, and install command. Treat an unverified environment as unsupported.

**2. `AgentDef`** in `src/agents.rs`: add to `AGENTS`, declaring detection, YOLO mode, hooks, lifecycle, and `SessionSupport` where resume argv is verified. Add a capture spec only for an authoritative source, naming one backend plus separate host and sandbox contexts. Hook-based capture must declare `HookIdentityField::SessionId` or `ConversationIdOrSessionId` from the upstream payload contract.

**3. Status detection**: an agent whose pane carries state gets a manifest in `src/tmux/detect/manifests/<agent>.toml` and a `detect_<agent>_status` calling into it (see `detect_claude`). Rules are `{id, state, priority, region, matcher}` and the highest-priority match wins, so a new case is a row rather than another branch. The hook file is a rule too (`region = "hook"`), which is what lets a blocking prompt on screen outrank a `running` write. Mark a rule `visible = true` only when it reads state off the agent's own live chrome, which is what lets the poller publish it without a confirming capture. Agents with no pane signal keep a stub returning `Status::Idle`.

**4. Hooks**: for non-Claude formats add an installer in `src/hooks/mod.rs` (see `install_hermes_hooks_with_events`), wire it into `SidecarHooks::install`, and add the agent to `status_hook_env_prefix()` so `AOE_INSTANCE_ID` and `AOE_PROFILE` reach the hook. Without the instance id, hooks write nothing. Use `HookStatus` rather than raw strings, and keep installers pure file IO: any subprocess work belongs in a separate function so `cargo test` cannot mutate a developer's real environment.

**5. Container mount**: add an `AgentConfigMount` (`tool_name`, `host_rel`, `container_suffix`, `skip_entries`) so the resolved config store is mounted where the containerized binary reads it, and install hooks and session-id sidecars into that mounted store. Declare a sandbox capture context only after this path is proven.

**6. Dockerfile**: install the agent and add its config dir to the `mkdir -p` block.

**7. Structured view profile**, if the agent ships an ACP server (its CLI accepts `acp` / `--acp`, or it ships a `*-acp` adapter): add the binary to `agent_registry.rs::with_defaults()` keyed on the `src/agents.rs` name, an install hint, a server profile registered in `resolve()`, and a mirrored frontend profile in `PROFILES`. Keep profiles conservative: until you have observed the adapter's `_meta` convention for child tool calls, leave `parent_meta_namespaces` and the alias map empty, since missing indentation is safer than fake parent links and an empty alias map renders the correct generic card.

**8. Tests**: update the registry matrix and settings round-trip tests, then cover resume argv, the capture backend, host and sandbox contexts, missing-id fail-closed behavior, `/clear` rotation where supported, restart persistence, and two concurrent sessions in the same cwd. Managed-store tests must prove the launch floor and ownership lease reject stale or peer-owned ids, and hook agents that the declared identity field reaches the pane-scoped sidecar.

**9. Verify**:

```bash
cargo fmt && cargo clippy -- -D warnings
cargo test --lib agents
cargo test --lib <youragent>
cargo test --lib container_config
cargo build && ./target/debug/aoe agents
```

## Hook format reference

### Claude and Gemini (generic `hook_config`)

Set `hook_config: Some(AgentHookConfig { ... })` and the generic `install_hooks()` handles their nested settings schema:

```json
{
  "hooks": {
    "PreToolUse": [{"hooks": [{"type": "command", "command": "sh -c '...'"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "sh -c '...'"}]}]
  }
}
```

Each `HookEvent` carries:

| Field | Meaning |
|-------|---------|
| `name` | The agent's event name, e.g. `"PreToolUse"`. |
| `matcher` | Optional pattern, for events that need one. |
| `status` | `Some(HookStatus::…)` installs a status writer on this event; `None` is a purely lifecycle event. |
| `identity_field` | Installs a command that extracts the declared top-level native identity from stdin into the pane-scoped `session_id` sidecar. Use only a field documented upstream. With `status` also set, the identity command runs first, and it stays installed when `agent_status_hooks = false`. |
| `waiting_tools` | Tool names whose invocation blocks on the user for the tool's whole execution (Claude's `AskUserQuestion`). The status writer then inspects the payload's `tool_name` and writes `waiting` instead of the event's status. Pair it with a tool-scoped event that restores the normal status, or the status sticks on `waiting`. |

### Other formats

- **Cursor Agent**: version 1 `.cursor/hooks.json`, with direct command entries under `hooks.beforeSubmitPrompt`. Its stable identity is `conversation_id`; `generation_id` is turn-scoped and must not be captured. Use `SidecarHooks` with `install_cursor_hooks_with_events`.
- **Codex**: the same generic JSON payload, written to `hooks.json` in Codex's config dir (`settings_rel_path: ".codex/hooks.json"`, `format: HookFormat::CodexJson`). `install_codex_json_hooks()` checks the adjacent `config.toml` feature opt-out first, and empty-event cleanup removes AoE's entries while preserving user hooks. Sandbox installation refuses linked or unreadable config files rather than following them. Codex's `config.toml` holds `[hooks.state]` trust data and `[features].hooks`; do not point `settings_rel_path` at it.
- **Hermes**: custom YAML, `hooks.pre_tool_call[].command`.
- **Kiro CLI**: a custom JSON agent config with `hooks.preToolUse[].command`.
- **Kimi Code**: a flat `[[hooks]]` array in `.kimi-code/config.toml`, which also holds provider and oauth settings, so the installer rewrites only its own entries.

## Common pitfalls

- **Missing `status_hook_env_prefix`**: without `AOE_INSTANCE_ID` hooks write nothing. Test by sending a message and checking `/tmp/aoe-hooks-$(id -u)/*/status` on the host, or `/tmp/aoe-hooks/*/status` inside the sandbox.
- **Sandbox hooks are separate**: host installation skips containers, so wire into `build_container_config` too.
- **Waiting status needs its own event**: not every agent exposes an approval event. Document the gap and consider filing upstream.
- **Sidebar quick permission response**: the TUI's `a` / `A` action needs each agent's exact keystroke sequence, not detection. Set `AgentDef.permission_response` only once you have confirmed by hand how the agent's prompt is answered (bare digit, arrow plus Enter, no assumed trailing Enter). Set `allow_always: None` when the prompt offers no "don't ask again" choice, and leave the whole field `None` until verified: the action then tells the user the agent is not supported yet.

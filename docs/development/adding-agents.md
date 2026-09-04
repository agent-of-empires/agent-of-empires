# Adding a New Agent

## Files touched

| File | Purpose |
|------|---------|
| `src/agents.rs` | Agent registry entry (name, binary, detection, flags) |
| `src/tmux/detect/manifests/<agent>.toml` | Detection rules, for an agent whose pane is parsed |
| `src/tmux/status_detection.rs` | Detection entry point (manifest call or stub) |
| `src/hooks/mod.rs` | Hook installer (if the agent supports hooks) |
| `src/session/instance/hooks.rs` | Wire hook installation + `AOE_INSTANCE_ID` env prefix |
| `src/session/config/container_config.rs` | Config mount for Docker sandbox |
| `src/acp/agent_registry.rs` | Structured view ACP adapter entry (only if the agent ships an ACP server) |
| `src/acp/agent_profiles.rs` + `web/src/lib/agentProfiles.ts` | Structured view profile (clear aliases, meta namespace, capability gates, tool aliases) |
| `src/acp/install_hints.rs` | Install hint surfaced by `aoe acp doctor` and handshake failures |
| `docker/Dockerfile` | Install agent in sandbox image |
| `docs/structured-view.md` | Per-agent structured view feature matrix |
| `README.md`, `docs/` | Documentation updates |

## Levels of support

Each level is additive; do only what the agent supports.

| Level | What it gives | Requires |
|-------|---------------|----------|
| 1. Basic | Appears in `aoe agents`, sessions launch, status always "Idle" | `AgentDef` + stub `detect_status` |
| 2. Pane-parse status | Status inferred from terminal output; no agent config | A manifest in `src/tmux/detect/manifests/`, plus a `detect_<agent>_status(&str) -> Status` calling into it |
| 3. Hook status | Agent writes status to a file via hooks; lands the instant state changes | `hook_config` + generic `install_hooks()`, or `sidecar_hooks` + a custom `install_<agent>_hooks()` |
| 4. Session resume | Restart resumes the same native conversation | A `session_support` contract with verified resume argv and, when available, an authoritative capture backend |
| 5. Docker sandbox | Runs isolated; host config synced in | `AgentConfigMount` + Dockerfile install |

Levels 3 and 4 are independent. `session_support` declares verified native resume argv. Its optional capture spec is the only backend allowed to supply an ID, with each environment explicitly `PaneScoped`, `Preassigned`, `ManagedExclusiveStore`, or `Unsupported`. Agents with verified resume argv but no authoritative identity source use `capture: None`: explicit `Use` and `Fork` IDs work, while automatic capture remains off. Agents without a verified native resume contract have no `session_support`. Managed-store capture requires a physically per-instance store, cwd match, launch-time floor, and cross-process ownership lease. `agent_status_hooks = false` disables status writers but not declared authoritative identity hooks.

## Steps

**1. Research:** binary name, detection (`which`), YOLO flag, exact native resume/fork argv, authoritative session-id source, host and sandbox storage paths, hook identity field, config dir, and install command. Treat an unverified environment as unsupported.

**2. `AgentDef` (`src/agents.rs`):** add to `AGENTS`. Declare detection, YOLO mode, hooks, lifecycle, and `SessionSupport` when native resume argv is verified. Add a capture spec only for an authoritative source; it names one backend plus separate host and sandbox contexts. Hook-based capture must declare `HookIdentityField::SessionId` or `ConversationIdOrSessionId` from the upstream payload contract. Store-backed capture is permitted only for an exact managed store with cwd filtering, a pre-launch time floor, and exclusive ownership. Use `capture: None` for argv-only support and omit `session_support` when resume argv itself is unverified.

**3. Status detection:** an agent whose pane carries state gets a manifest in `src/tmux/detect/manifests/<agent>.toml`, and a `detect_<agent>_status` that calls into it (see `detect_claude`). Rules are `{id, state, priority, region, matcher}` and the highest-priority match wins, so a new case is a row rather than another branch. The hook file is a rule too (`region = "hook"`), which is what lets a blocking prompt on screen outrank a `running` write, and what bounds how long an unrefreshed write keeps its authority. Give a rule `visible = true` only when it reads the state off the agent's own live chrome: that is what lets the poller publish it without waiting for a confirming capture. Agents with no pane signal keep a stub returning `Status::Idle`. See `src/tmux/detect/mod.rs` for the region vocabulary.

**4. Hooks (if applicable):** for non-Claude formats add a custom installer in `src/hooks/mod.rs` (see `install_hermes_hooks_with_events`, `install_kiro_hooks_with_events`). Wire it into `SidecarHooks::install`, and make sure `status_hook_env_prefix()` includes the agent so `AOE_INSTANCE_ID` and `AOE_PROFILE` reach the hook (without the instance id hooks write nothing). Hook statuses use `HookStatus` (`Running`, `Waiting`, `Idle`, `Error`), not raw strings, and sidecar event defaults live on the agent so profile `agents.<name>.status_map` entries feed host and sandbox installs through the same resolver. Keep installers as pure file IO; any subprocess work (e.g. setting a default agent) goes in a separate function so `cargo test` doesn't mutate the dev's real environment.

**5. Container mount (`src/session/config/container_config.rs`):** add an `AgentConfigMount` (`tool_name`, `host_rel`, `container_suffix`, `skip_entries`). The resolved config store must be mounted at the path the containerized binary actually reads. Install hooks and session-id sidecars into that mounted store. Add a sandbox capture context only after this path is proven; otherwise declare it `Unsupported`.

**6. Dockerfile (`docker/Dockerfile`):** install the agent and add its config dir to the `mkdir -p` block.

**7. Tests:** update the registry matrix and settings round-trip tests, then cover resume argv, capture backend, host/sandbox contexts, missing-id fail-closed behavior, `/clear` or `/new` rotation where supported, restart persistence, and two concurrent sessions in the same cwd. Managed store tests must prove the launch floor and ownership lease reject stale or peer-owned ids. Hook agents must prove the declared native identity field reaches the pane-scoped sidecar.

**8. Structured view profile (if the agent ships an ACP server):** its CLI accepts `acp`/`--acp` or ships a `*-acp` adapter. Add the binary to `src/acp/agent_registry.rs::with_defaults()` (keyed on the `src/agents.rs` name), an install hint to `src/acp/install_hints.rs`, a server profile to `src/acp/agent_profiles.rs` (registered in `resolve()`), and a mirrored profile in `web/src/lib/agentProfiles.ts` (registered in `PROFILES`). Keep profiles conservative: until you've observed the adapter's `_meta` convention for child tool-call linkage, leave `parent_meta_namespaces` and the alias map empty. Missing indentation is safer than fake parent links; an empty alias map renders the generic tool card, which is the correct fallback. Add the agent to the feature matrix in `docs/structured-view.md`; profile mechanics are documented in `docs/development/internals/structured-view.md`.

**9. Docs:** `README.md` (features + FAQ), `docs/index.md` (supported agents), `docs/guides/sandbox.md` (image table), `docker/Dockerfile.dev` (inherited-agents comment).

**10. Verify:**

```bash
cargo fmt && cargo clippy -- -D warnings
cargo test --lib agents
cargo test --lib <youragent>
cargo test --lib container_config
cargo build && ./target/debug/aoe agents   # verify detection
```

## Hook format reference

### Claude and Gemini (generic `hook_config`)

Set `hook_config: Some(AgentHookConfig { ... })`; the generic `install_hooks()` handles their nested settings schema.

```json
{
  "hooks": {
    "PreToolUse": [{"hooks": [{"type": "command", "command": "sh -c '...'"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "sh -c '...'"}]}]
  }
}
```

Each entry in `events: &[HookEvent]` carries:

| Field | Meaning |
|-------|---------|
| `name` | Agent's event name (e.g. `"PreToolUse"`). |
| `matcher` | Optional pattern for events that need it (e.g. Claude's `Notification` matcher). |
| `status` | `Some(HookStatus::Running\|Waiting\|Idle\|Error)` to install a status-writer on this event, or `None` for a purely lifecycle event. |
| `identity_field` | `Some(HookIdentityField::SessionId)` or `Some(HookIdentityField::ConversationIdOrSessionId)` installs a command that extracts the declared top-level native identity from stdin and writes it to the pane-scoped `session_id` sidecar. Use only a field documented by upstream. With `status` also set, the identity command runs first. Setting `agent_status_hooks = false` removes the status command but retains this identity command. |
| `waiting_tools` | Tool names whose invocation blocks on the user for the tool's entire execution (Claude's `AskUserQuestion`). When non-empty on a status event, the status writer inspects the payload's `tool_name` on stdin and writes `waiting` for these tools instead of the event's status. Pair it with a tool-scoped event that restores the normal status once the tool completes (Claude adds `PostToolUse` with matcher `AskUserQuestion`), or the status sticks on `waiting` through the rest of the turn. |

### Cursor Agent (flat `hooks.json`)

Cursor uses version 1 `.cursor/hooks.json`, with direct command entries under `hooks.beforeSubmitPrompt`. Its stable native identity is `conversation_id`; `generation_id` is turn-scoped and must not be captured. Use `SidecarHooks` with `install_cursor_hooks_with_events`, not the generic nested settings schema.

### Codex (`hooks.json`)

The generic JSON payload above, written to `hooks.json` in Codex's config dir rather than to a settings file: set `hook_config: Some(AgentHookConfig { settings_rel_path: ".codex/hooks.json", format: HookFormat::CodexJson, ... })`. `codex_hooks_json_path_in()` resolves `CODEX_HOME` (else `~/.codex`) and the generic `install_hooks()` writes it. Codex status weighs the hook write against its manifest rules by declared priority, so a prompt on screen outranks a `running` write.

Codex's separate `config.toml` stores `[hooks.state]` trust data and `[features].hooks = false`; its mutations are serialized with `config.toml.lock` and committed by atomic replacement. `install_codex_hooks_with_preserved_state()` / `uninstall_codex_hooks()` exist only for the v015/v017/v018 migrations that repair or strip hooks AoE once wrote there. Do not point `settings_rel_path` at `config.toml`.

### Hermes (custom YAML)

```yaml
hooks:
  pre_tool_call:
    - command: "sh -c '...'"
```

### Kiro CLI (custom JSON agent config)

```json
{
  "name": "aoe-hooks",
  "tools": ["*"],
  "hooks": {
    "preToolUse": [{"command": "sh -c '...'"}],
    "stop": [{"command": "sh -c '...'"}]
  }
}
```

### Kimi Code (custom TOML sidecar)

A flat `[[hooks]]` array in `.kimi-code/config.toml`, which also holds provider and oauth settings, so the installer rewrites only its own entries:

```toml
[[hooks]]
event = "PreToolUse"
command = "sh -c '...'"
```

## Common pitfalls

- **Missing `status_hook_env_prefix`:** without `AOE_INSTANCE_ID`, hooks write nothing.
- **Wrong hook format:** test that hooks fire by sending a message and checking `/tmp/aoe-hooks-$(id -u)/*/status` (host) or `/tmp/aoe-hooks/*/status` (inside the sandbox).
- **Sandbox hooks are separate:** host installation skips containers; wire into `build_container_config` too.
- **Waiting status needs a dedicated event:** not all agents expose an approval/permission event. If none exists, document it as a limitation and consider filing upstream.
- **Sidebar quick permission response:** the TUI's `a`/`A` sidebar action (respond to a pending permission prompt without attaching) needs each agent's exact keystroke sequence, not detection. Set `AgentDef.permission_response` to a `PermissionResponse { allow, allow_always, deny }` of `KeyToken`s (see `claude`/`opencode` in `src/agents.rs`) once you've confirmed, by hand, how the agent's own CLI prompt is answered (bare digit, arrow+Enter, etc., with no assumed trailing Enter). `allow_always` is an `Option`: set it to `None` when the agent's prompt offers no "don't ask again" choice (see `omp`), and the dialog drops the "Allow Always" button plus its `Shift+A` shortcut rather than offering one that would silently do nothing. Leave the whole `permission_response` field `None` if you haven't verified the sequences; the action then tells the user the agent isn't supported yet.

# Adding a New Agent

## Files touched

| File | Purpose |
|------|---------|
| `src/agents.rs` | Agent registry entry (name, binary, detection, flags) |
| `src/tmux/status_detection.rs` | Status detection function (pane parsing or stub) |
| `src/hooks/mod.rs` | Hook installer (if the agent supports hooks) |
| `src/session/instance.rs` | Wire hook installation + `AOE_INSTANCE_ID` env prefix |
| `src/session/container_config.rs` | Config mount for Docker sandbox |
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
| 2. Pane-parse status | Status inferred from terminal output; no agent config, brittle to UI changes | `detect_<agent>_status(&str) -> Status` (OpenCode, Vibe, Copilot, Pi, Droid) |
| 3. Hook status | Agent writes status to a file via hooks; reliable, survives UI changes | `hook_config` + generic `install_hooks()` or a custom `install_<agent>_hooks()` (Claude, Cursor, Gemini generic; Codex TOML, Hermes YAML, Kiro JSON) |
| 4. Session resume | Restart resumes the prior conversation | `resume_strategy` in `AgentDef` |
| 5. Docker sandbox | Runs isolated; host config synced in | `AgentConfigMount` + Dockerfile install |

## Steps

**1. Research:** binary name, detection (`which`), YOLO/auto-approve flag, resume flag, hook support + format (JSON/YAML/TOML), config dir, install command.

**2. `AgentDef` (`src/agents.rs`):** add to the `AGENTS` array. Key fields: `detection: DetectionMethod::Which(...)`, `yolo: Some(YoloMode::CliFlag(...))`, either `hook_config` (with `format: HookFormat::JsonSettings` or `HookFormat::CodexJson`) or `sidecar_hooks` (with `format: SidecarFormat::SettlToml`, `HermesYaml`, or `KiroJson`, plus `events: ..._SIDECAR_EVENTS`), `resume_strategy`, `host_only`, `install_hint`. The format enums drive installer and marker-walker dispatch; adding a hook-based agent without picking a variant is a compile error. `set_default_command: true` only when the binary name alone isn't enough to relaunch (e.g. opencode).

**3. Status detection (`src/tmux/status_detection.rs`):** hook-based agents get a stub returning `Status::Idle`. Pane-parse agents get a function matching on lowercased pane content. Prefer `--format json` over substring matching when the CLI offers it; human-readable output changes between versions.

**4. Hooks (if applicable):** for non-Claude formats add a custom installer in `src/hooks/mod.rs` (see `install_hermes_hooks_with_events`, `install_kiro_hooks_with_events`). Wire it into `SidecarHooks::install`, and make sure `status_hook_env_prefix()` includes the agent so `AOE_INSTANCE_ID` and `AOE_PROFILE` reach the hook (without the instance id hooks write nothing). Hook statuses use `HookStatus` (`Running`, `Waiting`, `Idle`, `Error`), not raw strings, and sidecar event defaults live on the agent so profile `agents.<name>.status_map` entries feed host and sandbox installs through the same resolver. Keep installers as pure file IO; any subprocess work (e.g. setting a default agent) goes in a separate function so `cargo test` doesn't mutate the dev's real environment.

**5. Container mount (`src/session/container_config.rs`):** add an `AgentConfigMount` (`tool_name`, `host_rel`, `container_suffix`, `skip_entries`). Host hook installation does not cover sandbox sessions; if the agent uses hooks, wire them into `build_container_config` so the sidecar volume mounts and config materializes in the container.

**6. Dockerfile (`docker/Dockerfile`):** install the agent and add its config dir to the `mkdir -p` block.

**7. Tests:** update the `src/agents.rs` tests (`test_get_agent_known`, `test_agent_names`, `test_resolve_tool_name`, `test_settings_index_roundtrip`, `test_send_keys_enter_delay`, `test_install_hint_lookup`); add a detection test in `status_detection.rs`; for hook-based agents add to `test_status_hook_env_prefix_includes_hermes`.

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

### Claude/Cursor/Gemini (generic `hook_config`)

Set `hook_config: Some(AgentHookConfig { ... })`; the generic `install_hooks()` handles it.

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
| `session_id_capture` | `true` installs a command that extracts `session_id` from the agent's stdin JSON and writes it to `/tmp/aoe-hooks-<euid>/<AOE_INSTANCE_ID>/session_id` (host) or `/tmp/aoe-hooks/<AOE_INSTANCE_ID>/session_id` (sandbox; see issue #1844 for the host/container path split), read by [session-resume](../guides/session-resume.md). Currently only Claude (`SessionStart`, `UserPromptSubmit`). With `status` also set, both commands share the matcher block and the session-id command runs first so it consumes stdin before the status writer. |

### Codex (custom TOML)

`[hooks]` table in `.codex/config.toml`:

```toml
[[hooks.PreToolUse]]
[[hooks.PreToolUse.hooks]]
type = "command"
command = "sh -c '...'"
```

Set `hook_config: Some(AgentHookConfig { settings_rel_path: ".codex/config.toml", ... })`. Host installs must go through `install_codex_hooks()` / `uninstall_codex_hooks()` so `CODEX_HOME`, existing `[hooks.state]` trust data, `[features].hooks = false`, the `config.toml.lock`, and atomic replacement are respected. Codex status is hook-first with targeted pane reconciliation for known hook gaps.

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

## Common pitfalls

- **Missing `status_hook_env_prefix`:** without `AOE_INSTANCE_ID`, hooks write nothing.
- **Wrong hook format:** test that hooks fire by sending a message and checking `/tmp/aoe-hooks-$(id -u)/*/status` (host) or `/tmp/aoe-hooks/*/status` (inside the sandbox).
- **Sandbox hooks are separate:** host installation skips containers; wire into `build_container_config` too.
- **Waiting status needs a dedicated event:** not all agents expose an approval/permission event. If none exists, document it as a limitation and consider filing upstream.

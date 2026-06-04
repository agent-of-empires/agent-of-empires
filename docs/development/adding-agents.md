# Adding a New Agent

This guide walks through adding support for a new AI coding agent to AoE.

## Overview

Adding an agent involves these files:

| File | Purpose |
|------|---------|
| `src/agents.rs` | Agent registry entry (name, binary, detection, flags) |
| `src/tmux/status_detection.rs` | Status detection function (pane parsing or stub) |
| `src/hooks/mod.rs` | Hook installer (if the agent supports hooks) |
| `src/session/instance.rs` | Wire hook installation + env prefix |
| `src/session/container_config.rs` | Config mount for Docker sandbox |
| `src/acp/agent_registry.rs` | Structured view ACP adapter entry (only if the agent ships an ACP server) |
| `src/acp/agent_profiles.rs` + `web/src/lib/agentProfiles.ts` | Structured view profile (clear aliases, meta namespace, capability gates, tool aliases) |
| `src/acp/install_hints.rs` | Install hint surfaced by `aoe acp doctor` and handshake failures |
| `docker/Dockerfile` | Install agent in sandbox image |
| `docs/acp/multi-agent.md` | Per-agent structured view feature matrix |
| `README.md`, `docs/` | Documentation updates |

## Levels of Support

Not all agents need everything. Here's what each level provides:

### Level 1: Basic (minimum viable)

- Agent appears in `aoe agents` list
- Sessions can be created and launched
- Status always shows "Idle"

Requires: `AgentDef` entry + stub `detect_status` function.

### Level 2: Status Detection via Pane Parsing

- AoE reads the terminal output and infers running/waiting/idle
- No agent-side configuration needed
- Can be brittle if the agent's UI changes

Requires: a `detect_<agent>_status(content: &str) -> Status` function that parses tmux pane content.

Examples: OpenCode, Vibe, Copilot, Pi, Droid.

### Level 3: Status Detection via Hooks

- AoE installs hooks into the agent's config that write status to a file
- Most reliable; survives UI changes
- Requires the agent to support a hook/event system

Requires: `hook_config` for the status events and config path, plus either the generic `install_hooks()` path for Claude-style JSON or a custom `install_<agent>_hooks()` function for another format.

Examples: Claude, Cursor, Gemini (generic), Codex (custom TOML), Hermes (custom YAML), Kiro (custom JSON).

### Level 4: Session Resume

- AoE can restart a session and resume the prior conversation
- Requires the agent to support a resume flag or session ID

Requires: setting `resume_strategy` in the `AgentDef`.

### Level 5: Docker Sandbox

- Agent runs in an isolated container
- Config is synced from host to container

Requires: `AgentConfigMount` entry + Dockerfile installation.

## Step-by-Step: Adding a New Agent

### 1. Research the Agent

Before writing code, gather:

- **Binary name**: What command launches it? (e.g., `kiro-cli`, `claude`, `codex`)
- **Detection**: How to check if it's installed? Usually `which <binary>`.
- **YOLO/auto-approve flag**: Does it have a flag to skip permission prompts?
- **Resume flag**: Can it resume a prior session? What flag?
- **Hook support**: Does it have lifecycle hooks (preToolUse, stop, etc.)?
- **Hook format**: If hooks exist, what's the config format? (JSON, YAML, TOML?)
- **Config directory**: Where does it store config? (e.g., `~/.kiro`, `~/.claude`)
- **Install command**: How do users install it?

### 2. Add the Agent Definition (`src/agents.rs`)

Add an entry to the `AGENTS` array:

```rust
AgentDef {
    name: "myagent",           // canonical name
    binary: "myagent-cli",     // binary to invoke
    aliases: &["my-agent"],    // alternative names for resolve_tool_name
    detection: DetectionMethod::Which("myagent-cli"),
    yolo: Some(YoloMode::CliFlag("--auto-approve")),
    instruction_flag: None,    // or Some("--system-prompt {}")
    set_default_command: false, // true if binary must be explicit in instance.command
    detect_status: status_detection::detect_myagent_status,
    container_env: &[],        // env vars for container sessions
    hook_config: None,         // or Some(AgentHookConfig { ... }) for Claude-format hooks
    resume_strategy: ResumeStrategy::Flag("--resume"),
    host_only: false,          // true if sandbox/worktree not supported
    send_keys_enter_delay_ms: 0,
    install_hint: "npm install -g myagent-cli",
},
```

### 3. Add Status Detection (`src/tmux/status_detection.rs`)

For hook-based agents, add a stub:

```rust
/// MyAgent status is detected via hooks, not tmux pane parsing.
pub fn detect_myagent_status(_content: &str) -> Status {
    Status::Idle
}
```

For pane-parsing agents, write a function that examines the terminal content:

```rust
pub fn detect_myagent_status(raw_content: &str) -> Status {
    let content = raw_content.to_lowercase();
    if content.contains("waiting for input") {
        Status::Waiting
    } else if content.contains("processing") {
        Status::Running
    } else {
        Status::Idle
    }
}
```

### 4. Add Hook Installation (if applicable)

If the agent supports hooks but uses a different format than Claude/Cursor/Gemini, add a custom installer in `src/hooks/mod.rs`. See `install_hermes_hooks` (YAML) or `install_kiro_hooks` (JSON) as examples.

Then wire it into `install_agent_status_hooks()` in `src/session/instance.rs`:

```rust
} else if self.tool == "myagent" && !self.is_sandboxed() {
    if let Some(home) = dirs::home_dir() {
        let config_path = home.join(".myagent/hooks.json");
        if let Err(e) = crate::hooks::install_myagent_hooks(&config_path) {
            tracing::warn!("Failed to install myagent hooks: {}", e);
        }
    }
}
```

And add the tool name to `status_hook_env_prefix()` so `AOE_INSTANCE_ID` is passed:

```rust
let has_hooks = agent.and_then(|a| a.hook_config.as_ref()).is_some()
    || tool == "settl"
    || tool == "hermes"
    || tool == "kiro"
    || tool == "myagent";
```

### 5. Add Container Config Mount (`src/session/container_config.rs`)

```rust
AgentConfigMount {
    tool_name: "myagent",
    host_rel: ".myagent",
    container_suffix: ".myagent",
    skip_entries: &["sandbox", "sessions", "cache"],
    seed_files: &[],
    copy_dirs: &[],
    keychain_credential: None,
    home_seed_files: &[],
    preserve_files: &[],
    clean_files: &[],
},
```

### 6. Add to Dockerfile (`docker/Dockerfile`)

```dockerfile
# Install MyAgent
RUN curl -fsSL https://myagent.dev/install | bash
```

And add the config directory to the `mkdir -p` block.

### 7. Update Tests

In `src/agents.rs`, update:
- `test_get_agent_known`
- `test_agent_names`
- `test_resolve_tool_name`
- `test_settings_index_roundtrip`
- `test_send_keys_enter_delay`
- `test_install_hint_lookup`

In `src/tmux/status_detection.rs`, add a test for your detection function.

In `src/session/instance.rs`, add your agent to `test_status_hook_env_prefix_includes_hermes` (if hook-based).

### 8. Add the Structured view Profile (if the agent has an ACP server)

If the agent publishes an ACP server (its CLI accepts `acp`, `--acp`, or
ships a separate `*-acp` adapter binary), wire structured view support too.

1. Add the binary to `src/acp/agent_registry.rs::with_defaults()`
   keyed on the same name you used in `src/agents.rs`.
2. Add an install hint to `src/acp/install_hints.rs::install_hint_for`.
3. Add a server-side profile to `src/acp/agent_profiles.rs`:

   ```rust
   pub const MYAGENT: AgentProfile = AgentProfile {
       key: "myagent",
       // Empty until you've observed the adapter's _meta convention for
       // child tool-call linkage. Don't guess; missing indentation is
       // safer than fake parent links.
       parent_meta_namespaces: &[],
       // Slash command(s) that reset the conversation. Empty if the
       // agent has no clear-boundary semantic.
       clear_aliases: &["/new"],
       supports_exit_plan_mode: false,
       supports_wakeup_tools: false,
   };
   ```

   Add the new constant to `resolve()` so `agent_profiles::resolve("myagent")`
   returns it.

4. Mirror in `web/src/lib/agentProfiles.ts`:

   ```ts
   const MYAGENT: AgentProfile = {
     key: "myagent",
     capabilities: { todos: false, skills: false, wakeup: false, subagents: false },
     parentMetaNamespaces: [],
     mcpPrefixes: ["mcp__"],
     // Mirror the Rust profile's `clear_aliases`. Empty if the agent
     // has no conversation-reset slash command.
     clearAliases: ["/new"],
     aliases: {
       execute: ["shell"],
       read: ["read_file"],
       edit: ["apply_patch", "edit_file"],
     },
     specialTitles: { todoPrefixes: [], skillNames: [], scheduleNames: [] },
   };
   ```

   Add it to `PROFILES` so `resolveAgentProfile("myagent")` returns it.

5. Document the agent in `docs/acp/multi-agent.md` (per-agent
   feature matrix + which structured view features work / fall back / fire).

Profile data should be conservative. When the adapter's tool surface
isn't verified, leave the alias map empty rather than guessing; the
structured view will render the generic tool card, which is the right fallback.

### 9. Update Documentation

- `README.md`: features list + FAQ
- `docs/index.md`: supported agents list
- `docs/guides/sandbox.md`: sandbox overview + image table
- `docker/Dockerfile.dev`: comment listing inherited agents

### 10. Verify

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test --lib agents
cargo test --lib <youragent>
cargo test --lib container_config
cargo build
./target/debug/aoe agents  # verify detection works
```

## Hook Format Reference

### Claude/Cursor/Gemini (generic `hook_config`)

```json
{
  "hooks": {
    "PreToolUse": [{"hooks": [{"type": "command", "command": "sh -c '...'"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "sh -c '...'"}]}]
  }
}
```

Set `hook_config: Some(AgentHookConfig { ... })` in the agent def; the generic `install_hooks()` handles it.

#### `HookEvent` flags

Each entry in `events: &[HookEvent]` carries:

- `name` — the agent's event name (e.g., `"PreToolUse"`).
- `matcher` — optional pattern for events that need it (e.g., Claude's `Notification` matcher).
- `status` — `Some("running" | "waiting" | "idle")` to install a status-writer command on this event, or `None` if the event is purely lifecycle.
- `session_id_capture` — `true` to install an additional command that extracts the active `session_id` from the agent's stdin JSON and writes it atomically to `/tmp/aoe-hooks/<AOE_INSTANCE_ID>/session_id`. Read by [`session-resume`](../guides/session-resume.md)'s poller as the primary capture source. Currently used only by Claude on `SessionStart` and `UserPromptSubmit`; other agents either don't pre-mint UUIDs (so capture is post-launch from disk) or use a different payload schema (Cursor, Codex). Setting both `status: Some(...)` and `session_id_capture: true` emits two commands under the same matcher block; the session-id command is placed first so it consumes stdin before the status writer.

### Codex (custom TOML)

Codex stores AoE status hooks in the `[hooks]` table in `.codex/config.toml`:

```toml
[[hooks.PreToolUse]]

[[hooks.PreToolUse.hooks]]
type = "command"
command = "sh -c '...'"

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = "sh -c '...'"
```

Set `hook_config: Some(AgentHookConfig { settings_rel_path: ".codex/config.toml", ... })` in the agent def. Host installs still use `install_codex_hooks()` so `CODEX_HOME`, existing `[hooks.state]` trust data, `[features].hooks = false`, `config.toml.lock`, and atomic replacement are respected. Codex status is hook-first, with targeted pane reconciliation for known hook gaps.

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

## Common Pitfalls

- **Forgetting `status_hook_env_prefix`**: Without `AOE_INSTANCE_ID`, hooks write nothing. Add your tool name to the check in `src/session/instance.rs`.
- **Wrong hook format**: Each agent has its own schema; test that hooks actually fire by sending a message and checking `/tmp/aoe-hooks/*/status`.
- **Sandbox hooks are separate**: Host hook installation (`install_agent_status_hooks`) doesn't cover sandbox sessions. You also need to wire hooks into `build_container_config` in `src/session/container_config.rs` so the sidecar volume is mounted and the agent config is materialized inside the container.
- **Do not bypass the Codex writer**: Codex config mutations must go through `install_codex_hooks()` or `uninstall_codex_hooks()` so AoE preserves trust state and avoids partial writes during concurrent launches.
- **Don't shell out in `install_*_hooks()`**: Keep hook installation as pure file IO. Any subprocess calls (like setting a default agent) should be in a separate function so `cargo test` doesn't mutate the developer's real environment.
- **Use structured output when parsing agent CLIs**: If the agent CLI has `--format json` or similar, use it instead of substring matching on human-readable output. Human-readable formats change between versions.
- **Waiting status needs a dedicated event**: Not all agents have an approval/permission event. If the agent doesn't expose one, document it as a limitation and consider filing upstream.
- **Redundant Dockerfile ENV**: Check if PATH is already set before adding another layer.
- **`set_default_command`**: Only needed for agents where the binary name alone isn't sufficient (e.g., opencode needs explicit command storage).

# Plugin API Reference

The field-by-field reference for `aoe-plugin.toml`, the manifest every Agent of Empires plugin ships. The schema lives in the `aoe-plugin-api` crate (`PluginManifest`) and is the source of truth; the host parses it strictly, so unknown keys are rejected and every key here maps to a schema field. For a guided introduction, see [Writing Plugins](development/writing-plugins.md).

## Versioning

A manifest carries two independent version axes.

| Key | Meaning |
|---|---|
| `api_version` | The manifest *schema* version. The current schema is `30`. The host rejects a manifest whose `api_version` is newer than it supports. |
| `aoe_version` | A semver requirement on the *host app* version, e.g. `">=1.11.0, <2.0.0"`. The host refuses to install, and skips loading, a plugin whose requirement excludes the running version. Optional; requires `api_version >= 4`. |

Each key below notes the `api_version` it needs. Target the newest schema your plugin uses, and set `aoe_version` to the host range you have tested.

## Top-level fields

```toml
id = "dev.example.my-plugin"
name = "My Plugin"
version = "0.1.0"
api_version = 30
aoe_version = ">=1.11.0, <2.0.0"
description = "What the plugin does."
capabilities = ["runtime.worker"]
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Plugin id (see [Plugin id](#plugin-id)). Namespaces config, events, and action names. |
| `name` | string | yes | Human-readable display name. |
| `version` | string | yes | Semantic version of the plugin. |
| `api_version` | integer | yes | Manifest schema version, `1` to `30`. |
| `description` | string | no | Shown in plugin listings. Defaults to empty. |
| `aoe_version` | string | no | Host-app semver requirement. Requires `api_version >= 4`. |
| `capabilities` | array of string | no | Runtime grants the worker needs (see [Capabilities](#capabilities)). A manifest with `[runtime]` must declare `runtime.worker`. Static contributions need none; provider-native agent status and replies require `session.integration`. |
| `command_groups` | array of tables | no | Named CLI verbs that nest their commands under `aoe <verb>`. Requires `api_version >= 27`. See [Command groups](#command-groups-api_version--27). |
| `screenshots` | array | no | Up to 8. Requires `api_version >= 5`. See [Screenshots](#screenshots). |
| `suggestions` | table | no | Suggested-session sources the worker publishes. Requires `api_version >= 23` and `runtime.worker`. See [Suggested sessions](#suggested-sessions). |
| `setting_defaults` | table | no | Overrides for core host settings, keyed by canonical path (e.g. `"theme.idle_decay_minutes"`). Resolution is user value, then plugin override, then core default. |
| `agent_integrations` | array of tables | no | Agent tools may declare provider-native communication or optional launch metadata. Requires `api_version >= 19`; the launch, detection, and reply-fallback extensions require API 21, the session-id capture extension requires API 22, and the `hooks` and `trust` tables require API 28, `display_name` and `icon` require API 29, `profile` requires API 30, each with a host implementing them. Provider-native status and replies require `session.integration`. |

## Agent integrations

Provider-native integrations connect a plugin to status and prompt replies for a terminal agent, separately from ACP and worker RPCs. The host owns session creation, approvals, and lifecycle. The user chooses each response; declaring an integration does not approve tools automatically.

An agent integration declares an optional launch command for an agent tool. Active plugins with a command contribute that tool to AoE's agent picker and CLI when its name is unique across plugin declarations and does not match a configured custom agent. Built-in names and aliases retain precedence under AoE's case-insensitive substring resolver, so a plugin tool containing any such token is omitted from the catalog and a warning names the plugin ID and tool.

Available plugin agents can be selected with `session.default_tool`. Discovery checks the executable after leading `NAME=value` assignments, using the process PATH and then the user's login shell without running the agent command or its arguments.

`command` is a launch line, not a shell script. Up to and including the executable it may contain only literal words separated by spaces or tabs: optional `NAME=value` assignments with valid environment names, followed by the executable. Quotes, backslashes, `$`, backticks, `~`, `#`, empty words, invalid assignment names, and anything the launch shell would act on in that prefix (`;`, `&`, `|`, parentheses, redirections, globs, braces, and whitespace such as a newline) are rejected when the manifest is validated, because discovery cannot reproduce a shell's expansion and field splitting closely enough to agree with the launch. The same rule refuses an executable beginning with `-`, which `env` would read as its own option. Arguments after the executable are passed through untouched and may use any syntax; the agent receives them from the shell exactly as written.

```toml
capabilities = ["session.integration"]

[[agent_integrations]]
tool = "session-helper"
provider_versions = ["^1.2"]
command = "session-helper"
replies = true
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `tool` | string | yes | Lowercase agent id; each tool may appear only once per plugin. |
| `provider_versions` | array of strings | conditional | Required for every entry when the manifest declares `session.integration`, including launch-only entries in a mixed manifest; nonempty valid alternative semver requirements for the agent runtime. A report's `provider_version` must match at least one. Omit only when the manifest declares no `session.integration` (launch-only agents). |
| `command` | string | no | Launch command: optional literal `NAME=value` assignments, then the executable, then free-form agent arguments. |
| `replies` | boolean | no | Explicitly enable provider-native prompt replies; defaults to false. Requires `session.integration`. Status-only integrations do not take over quick-response actions. |

Installing a plugin with agent integrations requires explicit approval. The install preview and CLI/TUI/web prompts show each tool, its command, and its agent declarations. Control and format characters are displayed as escapes. Updates require approval when an integration is added or any declaration changes; removals alone do not. AoE runs the declared command as the user only when that agent is launched.

The host matches status integrations to terminal sessions and rejects missing, malformed, or unsupported runtime versions before consuming reports or sending replies. A live provider that keeps reporting while naming a version outside, or unparseable against, `provider_versions` is the only case that steps aside: its report is not consumed and the session keeps pane detection rather than becoming an owned `Unknown`, with a warning naming the tool, the reported version, and the declared ranges. Any other absent, stale, or invalid report is still an owned `Unknown`: the declaration owns status once approved. The runtime extension must also enforce supported versions before activation. These requirements are independent of the plugin's `version`, AoE host `api_version` / `aoe_version`, and report/reply `schema_version`. Launch-only agent declarations do not enable provider-native status or replies.

For status, a session's own integration takes precedence; an unowned tool may use the integration selected by its detection alias. Reply ownership remains tied to the session's own tool.

### Launch, detection, and reply-fallback declarations (API 21)

These declarations require API 21 and a host that implements them. API 20 hosts reject them; API 20 agent integrations remain supported. Built-in agents and aliases retain precedence. Claude and OMP ship as bundled fallbacks. An approved, enabled launch declaration for `claude` or `omp` supersedes the bundled contract, including for existing sessions with no stored command, only when the plugin id is in a reserved namespace (`aoe.*` or `agent-of-empires.*`, which load only when featured or installed as a dev plugin) and the declaration has a `launch` table and a `command`; provider status and replies still require their capability and valid reports. If that declaration is absent, disabled, not approved, or from another namespace, the bundled launch, pane detection, and capture fallback remain available. A plugin-omp release declaring API 21 behavior is a separate prerequisite for the cutover.

Launch flags, instruction templates, environment values, install hints, and literal reply keys reject control and Unicode format characters.

```toml
capabilities = ["session.integration"]

[[agent_integrations]]
tool = "example-agent"
command = "example-agent"
provider_versions = ["^1.0"]
replies = false

[agent_integrations.launch]
aliases = ["example"]
oneshot_flag = "-p"
instruction_flag = "--prompt={}"
model_flag = "--model"
host_only = false
install_hint = "Install example-agent from its vendor"
send_keys_enter_delay_ms = 150

[agent_integrations.launch.container_env]
EXAMPLE_HOME = "/state"

[agent_integrations.launch.yolo]
kind = "flag"
flag = "--auto-approve"

[agent_integrations.launch.resume]
kind = "flag"
flag = "--resume"

[agent_integrations.launch.capture]
backend = "omp"
host = "pane_scoped"
sandbox = "unsupported"

[agent_integrations.detection]
mode = "native_only"

[agent_integrations.reply_fallback]
allow = [{ kind = "named", value = "Enter" }]
deny = [{ kind = "literal", value = "n" }]
```

The example's `detection` requires `session.integration` in the manifest's capabilities. Every declaration type, `session_id` included, requires a nonblank `command`: the plugin must contribute the tool it captures for, because only the plugin that owns a tool may attribute an id to a session using it.

- `launch.aliases` are unique lowercase agent ids, distinct from the tool. `oneshot_flag` and `model_flag` are single shell-safe flags. `instruction_flag` has one `{}` placeholder, as a whole argument or after `=`, and otherwise contains shell-safe words.
- `launch.send_keys_enter_delay_ms` is the pause, in milliseconds, that the host leaves between typing a message into the agent's pane and sending Enter. Some agents read a fast burst as a paste and drop the Enter, which leaves the message as an unsent draft that looks like a running session. It requires `api_version >= 22` and is capped at 2000. An agent that declares nothing falls back to the host's built-in pause for its tool.
- `launch.yolo` is `kind = "flag"` with `flag`, `kind = "always"`, or `kind = "env"` with `key` and `value`. An `always` declaration applies while active without changing the session's saved Yolo preference. `container_env` is applied to sandbox launches. Protected environment names are rejected.
- `launch.resume` is `kind = "flag"` with `flag`, `kind = "flag_pair"` with `existing` and `new_session`, or `kind = "subcommand"` with `command`. Resume and capture require each other. Capture supports only backend `omp`, with `pane_scoped` in at least one of `host` or `sandbox` and `pane_scoped` or `unsupported` in the other.
- `detection.mode = "native_only"` requires `session.integration`; the integration owns status rather than falling back to pane parsing when a report is missing or invalid.
- `session_id` declares that the plugin worker owns session-id capture for the tool, so the provider's store layout and wire formats stay in the plugin. It requires `api_version >= 22` and a manifest that declares a `[runtime]` worker. The declared worker reports what it captured with the `session.capture.set` RPC, taking `session_id` (the session to attribute) and `agent_session_id` (the provider's id) and requiring `session.write`. Only the plugin that owns the session's tool may report; any other caller gets `not_capture_owner`. The host applies the id through `persist_session_with_storage`, the guarded observation path the pollers use, and answers `applied = false` with one of three reasons: `pinned` (the row pins a different conversation for its next launch), `owned_elsewhere` (another session durably owns the id), or `stale` (the row changed since the host read it, or the id is capture-excluded).

A report is not tied to a launch. The host mints no launch token and the call carries none, so it cannot tell which launch produced the id, and a report that arrives late for an earlier launch is applied to whatever the row holds at that moment, subject to the reasons above. A worker must report only the id its current launch produced and stop reporting once that launch is gone. The id is also stored without an execution binding, because the report names no execution to scope it to: peer ownership and capture excludes are then judged over the whole id namespace rather than a single conversation's store. Polling, with its launch-scoped generations, is what will close both gaps. `method`, `launch_method` and `timeout_ms` are reserved for host-driven polling and are validated but not yet called by this host version: hosts do not poll the worker, apply a `launch_method` wrapper, or enforce `timeout_ms`, so a plugin must not rely on them yet. When they are implemented, `method` will be the per-poll worker RPC that returns `session_id` with the generation it was captured for, `launch_method` an optional RPC returning the launch wrapper and tmux environment the capture needs, and `timeout_ms` a per-call bound defaulting to 5000 and valid from 250 to 30000. Method names are dotted identifiers like `omp.session_id`.
- `reply_fallback` contains nonempty `allow` and `deny` arrays, and an optional nonempty `allow_always` array, of literal or named keys. Named keys are `Enter`, `Tab`, `Escape`, `Space`, `Backspace`, `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`, and `F1` through `F12`. Fallback is allowed only with `replies = false` and does not automatically approve a prompt.

For reply-enabled integrations, the provider report's `reply_keys` must contain at most eight distinct function-key names from `F1` through `F12`; the host rejects a report with more than eight keys or any duplicate. AoE sends those keys only after the user submits a response. A status-only report may omit `reply_keys` or send an empty array. Prompts are ignored unless the integration enables replies and the report supplies wake keys. Missing or invalid reports for reply-enabled integrations never fall back to terminal approval keystrokes.

### Agent profile (API 30)

`[agent_integrations.profile]` declares how the web dashboard and TUI render and drive the agent's structured view, with the field names of the host's built-in agent profile. It requires `api_version >= 30` and the entry's `command`. Every field is optional.

- Slash commands: `clear_aliases` and `compact_aliases`, each a single token starting with `/`; `clear_requires_driven_reset`.
- Capabilities: `supports_exit_plan_mode`, `supports_wakeup_tools`, `emits_heartbeat_keepalives`, `supports_subagents`, `todos`, `skills`, `mobile_monitor_only`.
- Names: `parent_meta_namespaces`, `subagent_tool_names`, `mcp_prefixes`, `yolo_mode_id`.
- Tool cards: `card_aliases` with `execute`, `read`, `edit`, `delete`, `search`, `fetch`, `think`; `special_titles` with `skill_names`, `schedule_names`, `harness_names`, `workflow_names`.
- `memory_path_regex`: a path regex that marks memory file operations. It must compile with the Rust `regex` crate and, because browsers run it, must not repeat a group (with `*`, `+`, `?` or `{n,m}`) that contains a quantifier or an alternation, and must not use inline flags, `(?P<` named groups or the `\A`, `\z` and `\Z` anchors.

Lists hold at most 32 unique, nonempty, printable strings of at most 128 bytes; `memory_path_regex` is at most 256 bytes. The agent key, fullscreen probe, one-shot capture patterns, native config agent, MCP env expansion and legacy mode fallback are not declarable.

### Display name and icon (API 29)

`display_name` and `icon` on an `[[agent_integrations]]` entry name the agent in pickers, `/api/agents`, the no-agents hint and web push titles; both require `api_version >= 29`. `display_name` is 1 to 40 printable characters without surrounding whitespace and defaults to `tool`. `icon` is a lucide kebab-case icon name. A plugin that takes over a bundled fallback shows "Replaces bundled <name> support" in its approval and update previews.

### Hooks and folder trust (API 28)

`[agent_integrations.hooks]` and `[agent_integrations.trust]` let a declared agent get the status hooks and pre-trusted folders a built-in agent has. Both require `api_version >= 28` and a nonblank `command`; a manifest below 28 that declares either is rejected. The host runs its own installer and writer, so the plugin supplies data, never code. The approval prompt shows the declared paths as `~/...` or `$VAR/...` templates, without resolving a session `agent_config_dir`. `aoe hooks status` lists the resolved hooks files, and the resolved trust files that are written only when `session.pre_trust_agent_folders` is on.

```toml
[agent_integrations.launch.container_env]
EXAMPLE_CONFIG_DIR = "/opt/example"

[agent_integrations.hooks]
settings_rel_path = ".example/settings.json"
config_dir_env_var = "EXAMPLE_CONFIG_DIR"
format = "json_settings"

[[agent_integrations.hooks.events]]
event = "PreToolUse"
status = "running"
waiting_tools = ["AskUserQuestion"]

[[agent_integrations.hooks.events]]
event = "Notification"
matcher = "permission_prompt"
status = "waiting"

[agent_integrations.trust]
file = ".example.json"
location = "config_dir_or_home"
config_dir_env_var = "EXAMPLE_CONFIG_DIR"
pointer = "/projects/{path}/hasTrustDialogAccepted"
value = true
sandbox = "yolo_only"
```

- `hooks.settings_rel_path` is a `.json` file inside a directory, relative to the home directory. When `config_dir_env_var` is set, the file's name is placed in that directory instead. Paths hold only plain components: no absolute paths, `.`, `..`, `:`, `,` or `\`, and at most 256 bytes. The host refuses to write a file that is another agent's settings file.
- `config_dir_env_var` (hooks and trust) is an ordinary launch variable name: not `HOME`, `PATH`, `AOE_*`, `LD_*`, `DYLD_*` or `XDG_*`, and not credential-shaped (containing `TOKEN`, `SECRET`, `KEY`, `PASSWORD`, `CREDENTIAL` or `AUTH`). It must also be a `launch.container_env` key. That value is the directory the sandbox mounts read-write, so it must be absolute and normalized, with no `..`, `:` or `,`, at most 256 bytes, and not `/`, `/root`, `/workspace*`, `/aoe*`, `/proc*`, `/sys*`, `/dev*` or `/tmp/aoe-hooks*`. The host checks it again at launch, and a directory that overlaps a mount it already binds is skipped with a warning rather than failing the sandbox.
- `hooks.format` is `json_settings`, the `hooks.<event>[].hooks[].command` layout. `events` holds 1 to 32 entries; each `event` is one of `SessionStart`, `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `StopFailure`, `Notification`, `ElicitationResult`, `PermissionRequest`, `BeforeTool`, `BeforeAgent`, `AfterAgent`. An event may repeat only with different `matcher` values. `status` is `running`, `waiting`, `idle` or `error`; `identity_field` is `session_id` or `conversation_id_or_session_id`; `waiting_tools` lists tools that block on the user. Each event needs a `status` or an `identity_field`. With any `identity_field`, the `command` must name a bare executable matching `[A-Za-z0-9._-]+`.
- `trust.file` is one `.json` file name, without `:`, `,` or `\`. The host refuses to write the default trust file of another built-in agent (`~/.claude.json`, Codex's `config.toml`, Gemini's `trustedFolders.json`). `location = "config_dir_or_home"` resolves to the session's `agent_config_dir`, else the `config_dir_env_var` directory, else the home directory. `pointer` is absolute, has exactly one `{path}` segment, and at most 8 segments. `{path}` receives the project path as a single object key, with no escaping. The value is written at the last segment. `value` is a boolean or a string.
- The hooks writer is strict too: a settings file that is not a JSON object is an error and is left untouched, never replaced. Built-in agents keep their lenient writer.
- The trust writer is strict. It creates missing objects but fails, without writing, when the file is not valid JSON or a segment on the pointer exists and is not an object. The host chooses whether to follow symlinks; a plugin cannot.
- `trust.file` must not be, or contain, the sandbox path of `hooks.settings_rel_path` (the same file, or a file named like the hooks directory), because the single-file trust mount would hide the staged hooks. Other files in that directory are fine.
- `trust.sandbox` is `always` or `yolo_only`; `yolo_only` writes the record only for a sandbox launched in Yolo mode. In a sandbox the host stages a per-session copy of the declared hooks file and trust file, so a declared agent needs no config mount of its own. A directory named by a `launch.container_env` entry for `config_dir_env_var` is where the container sees them, else the home directory. The staged files hold the AoE hooks and the trust record only; the host files are not copied. A sandbox write never follows a symlink. The staged directory lives as long as the session, so the agent's own files in it persist across relaunches. The staged trust file is reset when the declaration changes, so an entry written by an earlier declaration does not outlive it. The trust file is bind-mounted as one file, so an agent that replaces it by rename inside the container fails with `EBUSY`. On the host the trust record is written only when `session.pre_trust_agent_folders` is on, as for built-in agents.
- Uninstalling AoE removes the hooks from the declared settings file, including one relocated by `session.agent_config_dir`, while the plugin is active. Hooks left behind by a plugin that was removed or disabled before the uninstall stay in place. Trust entries stay, as built-in ones do. Host trust entries written by an earlier declaration are not retired when the declaration changes.

### Report and reply files

The provider writes `integration-<plugin id>.json` in the session's private hook directory, `/tmp/aoe-hooks-<effective uid>/<instance id>/`. AoE reads that report and writes `integration-<plugin id>.reply.json` in the same directory after the user responds. Both files must be regular files owned by the host's effective uid with exact mode `0600`; reports are limited to 64 KiB and rejected when their modification time is more than 15 seconds old or in the future. Providers should refresh reports at least every 15 seconds. `writer_pid` must identify a live process while the report is consumed.

AoE owns report validation and reply creation. The provider owns reading and removing its reply file after processing it; AoE replaces it on the next response and removes the per-instance directory during hook-status cleanup. This is a same-user trust boundary: file ownership and a live `writer_pid` do not authenticate a provider against another process running as the same uid. When `replies = false`, AoE drops any prompt from the report and does not offer provider-native replies.

The snapshot's `revision` changes only when the prompt changes, not when runtime status changes. Single-choice questions require exactly one selected option or a nonblank custom answer. Enter selects the highlighted option for an unanswered single-choice question before advancing or submitting; custom answers and multi-choice selections are preserved. The response dialog stays open for incomplete answers. While editing a custom answer, the viewport follows the input cursor.

Shell-hook sidecars and native-plugin reports remain distinct transports. Native reports retain their heartbeat and live-writer checks; shell hooks keep their existing freshness and publisher rules. Both reuse hardened host I/O and the established guarded session-identity persistence path where applicable. Declaring launch or reply metadata does not install hooks or companion extensions.

## ACP proxies

A proxy is a per-session process between the runner and an ACP adapter; the wire contract is in [the primitive contract](development/internals/acp-primitive-contract.md). The manifest field that declares one is not available yet (#5319), so the declaration source, its failure policy and its compat are not exposed to plugins. What a proxy process must do once launched:

- It runs in the agent's process group as the direct child of the runner. Its argv is its declared args followed by the full adapter argv, including the `docker exec` wrapper for a sandboxed session. It must start that adapter argv unchanged as a child process in the same process group (no `setsid`, no daemonizing), relay the adapter's stdin and stdout, and stay running to add the hello and translate frames. It cannot `exec` into the adapter. The runner kills the group, which is how the adapter dies with it. If the runner is killed with SIGKILL, the adapter can be orphaned.
- `AOE_PROXY_LAUNCH_CONFIG` holds the path of an owner-only JSON file, removed when the session ends: `{"version": 1, "session_id": "...", "container_id": "..." | null, "store_root": "..."}`. It holds no secrets. Strip the variable from the adapter's environment.
- For a sandboxed session the container environment travels on file descriptor 9, which the runner passes to the proxy and the adapter argv names as `--env-file /dev/fd/9`. The proxy must pass descriptor 9 to the adapter's `docker exec` as descriptor 9 (not remapped to another number) and must not close or read it.
- The `initialize` response must carry the hello with the declared `plugin_id`, and forward the adapter's own `agent_info` unchanged so it can be checked against the declared compat. A missing or invalid hello, a different `plugin_id`, or an `agent_info` outside the compat counts as a hello failure.

A hello failure, a proxy that cannot start or one that exits before its hello follows the declared failure policy. `refuse` fails the session start. `neutral_boot` launches the adapter once without the proxy (on the first launch and on a respawn), posts a "features unavailable" notice, and keeps that state across a reattach. Failures after a valid hello never fall back; see [Failure](development/internals/acp-primitive-contract.md#failure).

A proxy is trusted as the user: it can run anything the user's `docker` can, not only the adapter it was handed, and reads the filesystem with the user's rights ([ADR 0006](adr/0006-meta-aoe-primitive-contract.md)).

## Plugin id

A dotted, lowercase ASCII identifier such as `dev.example.review-helper`. Each dot-separated segment starts with a lowercase letter and may contain digits and hyphens; the whole id is at most 64 bytes. The `aoe.*` and `agent-of-empires.*` namespaces are reserved for bundled and officially featured plugins; a community install cannot claim them.

## Capabilities

Capabilities gate plugin resource access and host interactions. They are prompted once at install and pinned to the manifest hash, so an update that widens them must be re-approved. Declare only the capabilities the plugin uses; static contributions (commands, keybinds, themes, ui, status) need none.

| Capability | Grants |
|---|---|
| `runtime.worker` | Launching a plugin worker and using its base host RPCs. Required for either runtime kind. |
| `session.read` | Reading sessions of the host's profile: the attached session, `sessions.list`, `projects.list` (`api_version >= 23`, the registered projects), and the `session.status.changed` (`api_version >= 14`) and `session.lifecycle.changed` (`api_version >= 19`) notifications. |
| `session.write` | Mutating any session of the profile: `session.meta.set`/`cas`, `session.capture.set` for a tool the plugin owns and declares `session_id` for, and archive/trash from a plugin page. |
| `config.read` | Reading host or other-plugin configuration (not the plugin's own settings). |
| `config.write` | Writing host or other-plugin configuration. |
| `process.spawn` | Spawning processes beyond the plugin's own worker. |
| `net` | Outbound network access. |
| `fs.read` | Filesystem reads outside the plugin directory. |
| `fs.write` | Filesystem writes outside the plugin directory. |
| `clipboard.read` | Reading the clipboard. |
| `clipboard.write` | Writing the clipboard. |
| `notifications` | Posting desktop / TUI notifications. |
| `browser_open` | Opening a URL in the user's browser from a command `action`. |
| `composer.read` | Reading a click-scoped snapshot of the active ACP composer draft from a `composer-action`. |
| `composer.write` | Publishing a host-validated draft edit from a `composer-action` UI-state payload. |
| `acp.capabilities.read` | Discovering available agents and their advertised models/modes via `acp.capabilities.get` (`api_version >= 9`). |
| `acp.capabilities.probe` | Triggering a handshake-only catalog probe via `acp.capabilities.probe`: the host spawns the agent adapter, runs initialize + `session/new` (no prompt turn, so no tokens), records the advertised models/modes/thought-levels, and tears it down. Distinct from `acp.capabilities.read` because it spawns a real process (`api_version >= 11`). |
| `session.create` | Creating a host-owned structured session via `sessions.create` (`api_version >= 9`). |
| `session.prompt` | Delivering a turn to a session the plugin created via `sessions.turn.send`, and the initial turn on `sessions.create` (`api_version >= 9`). |
| `session.unattended` | Creating a session in a host-classified *unattended* approval mode. A distinct, high-severity grant, never implied by `session.create` or `session.prompt` (`api_version >= 9`). See [Session-driving RPCs](#session-driving-rpcs). |
| `session.message` | Pushing a text message into **any** session, including ones the plugin did not create, via `sessions.message.send`. A structured session shows the message as sent by the plugin, and the call reaches sessions the plugin did not create, so it is a separate grant, never implied by `session.prompt` (`api_version >= 15`). |
| `session.integration` | Provider-native status and user-requested replies for an agent (`api_version >= 19`). Launch-only agent metadata does not require this capability. |
| `mcp.rewrite` | Serving the `mcp.rewrite` hook. The worker receives each session's resolved MCP server definitions **including secret env and header values**, and returns the set AoE forwards to the agent (`api_version >= 24`). A distinct, high-severity grant, never implied by another. See [Hooks](#hooks). |
| `mcp.serve` | Serving MCP servers from the plugin's worker (`api_version >= 26`, requires a `[runtime]` worker). Gives the plugin an authenticated per-session MCP endpoint, so it can answer tool calls an agent makes in that session; the worker never sees the credentials. A distinct, high-severity grant, never implied by another. |

A capability this host version does not recognize is rejected, not granted.

## Commands

Palette entries, namespaced by the host as `plugin.<id>.<command-id>`. A command with no `action` is delivered to the worker as a `plugin.command.invoke` notification. The CLI lists them in `aoe --help` but cannot run them yet. A command whose name a core subcommand or an earlier plugin already uses is not listed. A command that names a `group` appears as `aoe <group> <id>` instead of a flat top-level verb.

```toml
[[commands]]
id = "status"
title = "My Plugin: status"
description = "Show the status summary."
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Command id. Empty is unaddressable. |
| `title` | string | no | Display name. |
| `description` | string | no | Help text. |
| `action` | table | no | A client-executed action. Requires `api_version >= 6` and the `browser_open` capability. |
| `group` | string | no | Id of a declared `[[command_groups]]` entry. Requires `api_version >= 27`. See [Command groups](#command-groups-api_version--27). |

A command `action` is a client-executed action instead of a worker call. The only `kind` is `open-ui-link`, which opens the `href` from the plugin's own `(slot, id)` UI-state entry in the browser, with no worker round-trip; that pair must match a declared `[[ui]]` entry on a per-session slot.

```toml
[commands.action]
kind = "open-ui-link"
slot = "row-badge"
id = "my_badge"
```

`href` may also be a same-origin relative path, which is the only portable way
to link back into aoe (a plugin cannot know the host, port, or tunnel origin).
Relative hrefs must start with exactly one `/`; `//host`, backslashes, and
embedded tab/CR/LF are rejected because a browser would resolve them onto a
different origin. See [Link targets](#link-targets).

### Command groups (`api_version >= 27`)

A group gives the plugin a short CLI verb instead of a name-prefixed one.
Declare the verb once and set `group` on the commands it carries:

```toml
[[command_groups]]
id = "linear"
about = "Linear issue commands"

[[commands]]
id = "refresh"
title = "Refresh Linear issues"
group = "linear"
```

That grafts `aoe linear refresh`. A `[[commands]]` entry with no `group` keeps
its flat top-level name, so groups are additive and one level deep.

`id` on a group, and on a command that names one, is a single bare lowercase
verb (letters, digits, `-`, `_`), and `help` is reserved for the group's own
help subcommand. Nesting changes only the CLI path: the command id, the
`plugin.<id>.<command-id>` name the host sends in `plugin.command.invoke`
params, `action` targets, and `[[keybinds]]` stay as they are.

Core keeps the top level. A group whose id matches a core subcommand is not
grafted, and neither are its commands; a group id goes to the first active
plugin that declares it, and within a group the first plugin to claim a leaf
name wins. A flat command whose name an accepted group verb already takes is not
grafted.

## Keybinds

```toml
[[keybinds]]
command = "status"
key = "Ctrl+Shift+G"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `command` | string | yes | Target command id (a plugin or core command). |
| `key` | string | yes | Key chord, e.g. `Ctrl+Shift+G`. Core bindings win a collision. |

## Settings

Plugin-declared settings, rendered on the TUI and web settings surfaces and stored under `[plugins."<id>".settings]`. The worker reads them via the `config.get` host RPC.

```toml
[[settings]]
key = "refresh_secs"
label = "Refresh interval (seconds)"
type = "integer"
default = 120
min = 0
max = 86400
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `key` | string | yes | Setting key, stored under the plugin's settings table. |
| `label` | string | no | Display label. |
| `description` | string | no | Help text. |
| `type` | string | no | Value type (see below). Defaults to `string`. |
| `options` | array of string | no | Allowed values for `select`; ignored otherwise. |
| `min` / `max` | integer | no | Inclusive bounds for `integer`; ignored otherwise. |
| `default` | any | no | Declared default. Must match `type`. Absent means the type's zero value. |
| `advanced` | bool | no | Group under the Advanced fold. Defaults to `false`. |
| `multiline` | bool | no | Render a `string` field as a multi-line textarea; ignored for other types (`api_version >= 11`). |
| `option_source` | string | no | Host source for a `dynamic_select` (`api_version >= 9`). |
| `depends_on` | array of string | no | Sibling keys whose values parameterize a `dynamic_select` (`api_version >= 9`). |
| `fields` | array | no | Item fields of an `object_list` (`api_version >= 9`). |
| `item_id_key` | string | no | Item field holding each `object_list` row's stable id; defaults to `_id` (host-generated) (`api_version >= 9`). |
| `min_items` / `max_items` | integer | no | Inclusive item-count bounds for an `object_list` (`api_version >= 9`). |
| `per_project` | bool | no | Also settable per registered project, falling back to the global value. Defaults to `false` (`api_version >= 18`). See [Per-project settings](#per-project-settings). |

Setting types:

| `type` | Widget |
|---|---|
| `string` | Text input (default). |
| `bool` (or `boolean`) | Toggle. |
| `integer` | Number input, bounded by `min` / `max`. |
| `select` | Dropdown over a non-empty `options` array. |
| `dynamic_select` | Dropdown whose choices the host resolves from `option_source` (`api_version >= 9`). |
| `dynamic_multi_select` | Multi-select (checkbox list) whose choices the host resolves from `option_source`; the stored value is an array of chosen values. Object-list item fields only (`api_version >= 11`). |
| `cron` | Validated 5-field cron expression text field (`api_version >= 9`). |
| `object_list` | A repeatable list of structured items described by `fields` (`api_version >= 9`). |
| `string_list` | Freeform add/remove list of user-typed strings; no closed option set. Top-level setting or object-list item field (`api_version >= 14`). |

### Dynamic selects (`api_version >= 9`)

A `dynamic_select`'s options are resolved by the **host** at render time, so the plugin never ships a list that could drift from the host's real agents, models, or projects. Set `option_source` to one of:

| `option_source` | Choices |
|---|---|
| `acp.agents` | ACP-capable agents the host knows *and whose adapter is installed on this host*. Uninstalled harnesses are not offered. |
| `acp.models` | Models the selected agent advertised. Needs the agent via `depends_on`. |
| `acp.modes` | Approval modes the selected agent advertised. Needs the agent via `depends_on`. |
| `projects` | Registered projects (value is the project path). |
| `groups` | Existing session group paths. |

`depends_on` names sibling keys whose values parameterize the source, which `acp.models` and `acp.modes` require. When that agent's catalog has never been discovered, resolving them runs a one-shot handshake probe (see `acp.capabilities.probe`), so the picker self-fills on first open. Saved ids are advisory: the host revalidates at session creation, so a model that later disappears surfaces as an error then rather than silently at save.

### Object lists (`api_version >= 9`)

An `object_list` is a repeatable list of structured records (a scheduler's entries, say), stored as a TOML array of tables under `[[plugins."<id>".settings.<key>]]`. It is **one level deep**: item fields are declared in `fields` and cannot themselves be an `object_list`. Every item carries a stable id under `item_id_key`, host-generated on add and never changed on edit or reorder, so a worker can track an entry across edits.

```toml
[[settings]]
key = "jobs"
type = "object_list"
item_id_key = "id"
max_items = 50

[[settings.fields]]
key = "agent_id"
type = "dynamic_select"
option_source = "acp.agents"
required = true

[[settings.fields]]
key = "schedule"
type = "cron"
required = true
```

An item field takes the same keys as a top-level setting (`key`, `label`, `description`, `type`, `options`, `min`, `max`, `default`, `multiline`, `option_source`, `depends_on`) plus `required`. It may be a `dynamic_multi_select` (`api_version >= 11`), whose stored value is an array of the chosen option values, or a `string_list` (`api_version >= 14`), whose stored value is an array of freeform user-typed strings.

### Per-project settings

A top-level setting with `per_project = true` (`api_version >= 18`) can also be set for a registered project. The value is stored in that project's registry entry (`projects.json`), validated like a global write, and edited in the web project form. A profile-scoped registry entry shadows a global one for the same path.

A worker reads the resolved value with `config.get`, passing the project's repository path:

```json
{ "key": "depth", "project_path": "/home/me/work/repo" }
```

The result is `{ "value", "scope", "revision" }`. `scope` is `"project"` when the project sets the key, else `"global"` when the global value is set, else `"default"` for the manifest `default`; `value` and `scope` are `null` when none applies. Without `project_path`, or for a path that is not a registered project, the project step is skipped. A key that is not `per_project` only ever returns its stored global value, with no `default` fallback and `project_path` ignored.

Writing a per-project value sends `plugin.settings.changed` with the usual `{ revision, changed_keys }` plus the registry's `project_path`. Re-read those keys with `config.get` and that `project_path`.

## Session listing

`sessions.list` (capability `session.read`) returns `{ "sessions": [...] }`. The optional `exclude` param is an array of `archived`, `snoozed` and `trashed`; with none, every stored session is returned. A trashed session stays present with `trashed: true`; a permanently purged session is absent.

| Field | Type | Meaning |
|---|---|---|
| `id`, `title`, `tool`, `status` | string | Identity and current status. |
| `project_path` | string | The session's own working directory; for a worktree session this is the worktree, not the main repo. |
| `archived`, `snoozed`, `trashed` | bool | Lifecycle flags. |
| `unread` | bool | The session carries the unread marker; always `false` while `session.unread_indicator` is off. |
| `workspace_repos` | array | One `{ source_path, branch, worktree_path }` per repo checkout of a multi-repo workspace; empty otherwise. `branch` is the branch recorded at workspace creation and can be stale; read `worktree_path` for the live checkout. |
| `worktree` | object | `{ branch, main_repo_path, managed_by_aoe }`; present only for worktree sessions. |

`trashed`, `unread`, `workspace_repos` (and its `worktree_path` entries) and `worktree` were added after the first release; they are additive RPC fields and do not change the manifest `api_version`. Treat a missing key as absent on an older host.

## Session-driving RPCs

With `api_version >= 9` a worker can discover ACP capabilities and create host-owned structured sessions, the primitives an automation plugin (for example a scheduler) needs. These are worker RPCs, not manifest keys; the host enforces a strict security model around them.

| Method | Capability | Purpose |
|---|---|---|
| `acp.capabilities.get` | `acp.capabilities.read` | List agents and their advertised models / modes / thought-levels (never launches an agent; a never-run agent reports `catalog_status: undiscovered` with empty lists). |
| `acp.capabilities.probe` | `acp.capabilities.probe` | Populate the catalog for one agent (optional `agent_id`; otherwise every undiscovered registry agent) via a handshake-only probe, then return the same shape as `acp.capabilities.get`. Spawns the adapter and runs initialize + `session/new` with **no prompt turn** (no tokens); each probe degrades to a no-op on failure. `api_version >= 11`. |
| `sessions.create` | `session.create` (+ `session.prompt` for an initial turn, + `session.unattended` for an unattended mode) | Create a structured session, optionally with an initial turn and a plugin-scoped idempotency key. |
| `sessions.turn.send` | `session.prompt` | Deliver a turn to a session **this plugin created**. |
| `sessions.message.send` | `session.message` | Deliver a short text message to **any** session, like `aoe send`. See [Messaging any session](#messaging-any-session). `api_version >= 15`. |
| `plugin.storage.get` / `set` / `cas` / `remove` | `runtime.worker` | Plugin-private durable key/value storage (see [Plugin storage](#plugin-storage)). |
| `suggestions.set` / `error` / `sources` / `checks.set` | `runtime.worker` | Publish suggested sessions (see [Suggested sessions](#suggested-sessions)). `api_version >= 23`. |
| `projects.list` | `session.read` | Registered projects of the host profile, `{ projects: [{ name, path }] }`. `api_version >= 23`. |

**Project selection (`api_version >= 11`).** `sessions.create` takes an optional `project_path` (the trust-checked primary repo) and `extra_project_paths` (the other repos of a multi-repo session). Omitting `project_path` creates a **scratch** session: a throwaway directory with no repository, so extras alongside it are refused. Every path is canonicalized and existence-checked host-side, fail-closed and capped per call.

**Sandbox (`api_version >= 11`).** `sandbox: true` runs the session in the host's container sandbox, on the host's own configured image. It only narrows what the agent can reach, so it needs no grant beyond `session.create`. The create fails synchronously when no runtime is installed, but the container starts asynchronously, so image-pull problems surface on the session later.

**Approval-mode classification.** The plugin proposes a `mode_id`; the **host** decides its security class. A mode is *interactive* (omitted, adapter default), *guarded* (a reviewed read-only or plan preset), or *unattended* (a bypass or auto-write mode, plus every mode the host does not recognize, which fail closed). An unattended mode requires `session.unattended` on top of `session.create`.

**Repository trust holds regardless of grants.** A session against a repository whose hooks need approval is refused even with `session.unattended`; a plugin cannot pre-approve trust. See [Unattended sessions](development/internals/plugin-system.md#unattended-plugin-sessions).

**Ownership.** `sessions.turn.send` reaches only a session the calling plugin created. To reach any other session, use `sessions.message.send`.

**Busy sessions.** A turn aimed at an agent already running a non-steerable turn (or cancelling, or compacting) is refused with a retryable `agent_busy` rather than dropped. So is a `/compact` or `/clear` aimed at any running turn, steerable or not, since such a command runs on its own turn. A stopped or dormant session is not busy: the host wakes it the way a user prompt does, closes any turn the previous worker left open, resumes the worker, and waits. An archived or trashed session is never woken; the turn fails with `session_archived` or `session_trashed`. An ignored session fails with `ignored` and keeps its marker.

**Idempotency.** `sessions.create` takes a plugin-scoped `idempotency_key`: retrying with the same key and payload returns the existing session (`created: false`), while a different payload under that key is a conflict.

**Limits.** Per plugin: 20 creates per hour, 5 active plugin-created sessions, 120 turns per hour, 600 messages per hour, reported as `rate_limited` or `concurrency_limited`. Disabling the plugin stops all of its automation.
A pending revival occupies an active slot until a real counted or terminal
transition settles it. Metadata reloads and prompt recency writes do not release that slot.

**Settings-change events.** After a settings write the host notifies the worker with `plugin.settings.changed` carrying `{ revision, changed_keys }`; the worker re-reads those values with `config.get`, whose response carries the current `revision`. Polling that method is the fallback for a worker that was down.

**Session status events.** With `api_version >= 14` and the `session.read` capability, the host notifies the worker with `session.status.changed` on every status transition of a session in the host's profile (the same sessions `sessions.list` returns), for structured and terminal sessions alike. The params are `{ session_id, title, from, to, at }`: `from` and `to` use the same PascalCase spelling as `sessions.list` (`Running`, `Idle`, `Waiting`, `Stopped`, `Error`, and so on), and `at` is an RFC 3339 timestamp. The host sends the transitions it processes, so filter on `to` (for example `Idle` for "the agent stopped working"). Do not treat the notifications as a complete transition log: a status change made by another process, such as the CLI or TUI, is reported when the daemon next reloads it, changes within one poll tick can merge into one event, and delivery is best-effort as described below. It is a best-effort, fire-and-forget notification: do not reply, and an event is never replayed. It is lost if the worker was down when it fired, if the daemon's status stream lagged and skipped it, or if the worker had stopped reading stdin (at most 64 unread status events are queued per worker; later ones are dropped until it catches up). Resynchronize from `sessions.list` on startup and whenever you may have missed events, but it reads the stored sessions: the `status` of a structured (ACP) session moves on the live row without being persisted each time, so the listed value can be older than the live one. There is no live-status snapshot call yet.

Status notifications follow canonical row adoption. Rejected or stale operations emit no status transition, and status polling does not resend a transition already published by that commit.

**Session lifecycle events.** With `api_version >= 19` and the `session.read` capability, the host also notifies the worker with `session.lifecycle.changed` when a session in the host's profile changes lifecycle state. The params are `{ session_id, kind, at, summary }`. `kind` is `created`, `deleted`, `archived`, `unarchived`, `trashed`, `restored`, `renamed` or `workspace_changed` (the set of checkouts the session works in changed; a branch switch alone does not count), and `at` is an RFC 3339 timestamp. `summary` is the session's `sessions.list` entry at the time of the change. The events keep membership and the listed lifecycle fields current; other fields such as `snoozed` and branches can change without an event, so re-read `sessions.list` when you need their current values. A `deleted` event has no `summary`: the session is gone. The events follow the host's profile, like `sessions.list`: a session moved to another profile is `deleted` for a plugin in the old profile and `created` for one in the new profile. One change can produce several events, such as `archived` and `trashed` together. These events share the delivery guarantee and the 64-event queue of the status events above, and are never replayed. They are best-effort: a change made by another process (the CLI or TUI) is detected from the session store a moment later, and changes inside that window are coalesced, so a session created and deleted in quick succession may produce no event. A session created while the daemon was not running produces no `created` event. Resynchronize from `sessions.list` on startup and whenever you may have missed events.

## Messaging any session

`sessions.message.send` takes `{ session_id, message }` and delivers `message` to the session. For a structured session the message is marked as sent by the plugin, never as typed by the user: the transcript row and a queued message carry the plugin id, and the web and TUI show it. A terminal session receives the message as plain terminal input, with no attribution. It is for pings such as a PR notification, not for driving a session: it cannot create sessions or read their output, and there is no idempotency key.

**Result.** `{ disposition, revived }`. `disposition` is `sent`, `steered` or `queued`. A structured session takes the same path as the web composer: a running agent that accepts steering takes the message mid-turn (`steered`), one that does not has it parked on the session's queue until the turn ends (`queued`), and an idle one starts a turn (`sent`). A busy agent is never refused. A terminal session always reports `sent`. `revived` is `true` when the host had to start something to take the message. For a structured session that is a stopped, dormant or snoozed worker, which is resumed and given the message immediately rather than queued, unless it is a sandbox waiting on its agent's sandbox login (`sandbox_login_required` below). For a terminal session it is a stopped or dead pane that was started or respawned. A snoozed terminal keeps its live pane, so it reports `revived: false`; the message still clears the snooze.

**Targets.** Any session, with no ownership check: the `session.message` grant is the gate. A structured session still runs the message under its persisted permission mode, as it does for turns from the plugin that created it. A woken session does not count toward the plugin's active-session limit, which only covers sessions the plugin created. Terminal (tmux) sessions are supported and a dead pane is revived first. In CityHall mode (`AOE_CITYHALL_MODE`) terminal sessions refuse plugin input with `terminal_unavailable`; this deliberately differs from `aoe send`, which does not check CityHall.

**Validation.** An empty or whitespace-only message is `invalid_params` (`message_empty`); one over 64 KiB is `invalid_params` (`message_too_long`).

**Errors.** All carry a stable `data.kind`:

| `data.kind` | Meaning |
|---|---|
| `session_not_found` | No such session, which is also what a hard-deleted session looks like. |
| `session_archived` | The session is archived; it is never woken. Skip it and keep the registration. |
| `session_trashed` | The session is in the trash; it is never woken. |
| `ignored` | The session is managed by another session and ignored; a plugin send is never delivered or allowed to clear the marker. A person must unignore it or reclaim ownership first. |
| `rate_limited` | More than 600 messages per rolling hour for this plugin. A bucket of its own, separate from turns. |
| `resume_failed` | A terminal session's agent could not resume. `data.resume_session_id` holds the preserved session id. |
| `session_not_running` | A terminal pane vanished between the revive and the send. |
| `session_transient` | A terminal session is mid-start or mid-stop; retry when it settles. |
| `input_line_busy` | A terminal session's input box held text a person had not sent for the whole 3 s wait; nothing was typed. Retry once they send or clear it. |
| `worker_not_ready` | A structured session's worker did not come up, or could not be resumed. Retryable. |
| `sandbox_login_required` | Resuming a sandbox needs the sandbox login of its agent (`aoe sandbox login --tool <tool>`, with `-p <profile>` when the session runs under a non-default profile and `--command <command>` when it runs its own command, as the message says). A structured session keeps the message in its queue (`data.queued_id`) and gets it after the login: do not retry. A terminal session has no queue: send again after the login. `sessions.turn.send` keeps a refused turn the same way. |
| `before_start_hook_failed` | The `host_hooks.before_start` hook of a sandbox session failed, timed out, could not be started or is still running for that session while its launch was being admitted. The message is kept (`data.queued_id`) and goes out with the next revival; no hold is registered, so nothing revives the session when the hook is fixed, and no plugin session cap reservation is left behind. The error text is fixed (it never carries the hook's command or output). |
| `terminal_unavailable` | CityHall mode refuses terminal targets. |
| `delivery_timeout` | The call did not finish within its deadline. The outcome is unknown: the message may still have been delivered. |
| `capability_missing` | The manifest lacks, or the user has not approved, `session.message`. |

**Timing.** The call is synchronous. It returns, or fails with `delivery_timeout`, within 25 seconds (a 10 second worker-ready wait plus 15 seconds for a terminal resume), so a worker may call it from a scheduler loop.

## Hooks

### `[hooks.mcp_rewrite]` (`api_version >= 24`)

Lets a worker rewrite the MCP servers AoE forwards to a session's agent. Requires the `mcp.rewrite` capability and a `[runtime]` worker. The hook runs only while the plugin is active: enabled, loaded, and with a grant covering the current manifest hash. While it is inactive (disabled, failed to load, or awaiting re-approval after an update changed its manifest) no hook runs and sandboxes get the full resolved set.

```toml
[hooks.mcp_rewrite]
applies_to = "sandbox"   # "sandbox" (default) or "all"
timeout_ms = 5000        # 1..=30000
order = 0                # lower runs first; ties break by plugin id
```

Plugins that inject servers should keep the default or a negative `order`. A plugin that must see everything the others add, such as a security proxy, should declare a high `order` (for example `1000`) so it runs last.

After AoE merges the MCP layers and before it hands the set to the agent, on every launch and respawn, the host sends the worker a `mcp.rewrite` request. It is a host-initiated JSON-RPC request whose `id` is a string of the form `h-<n>`. The worker answers with a JSON-RPC response carrying the same string `id` and either a `result` (the edit below) or an `error` object. A worker that is still starting is waited for until `timeout_ms` elapses.

```json
{
  "session_id": "…", "agent": "…", "sandboxed": true,
  "profile": null, "cwd": "/path",
  "sandbox": {
    "container_workdir": "/workspace/foo",
    "mounts": [ { "host": "/abs/host/foo", "container": "/workspace/foo", "read_only": false } ]
  },
  "servers": [
    { "name": "docs", "type": "http", "url": "https://…", "headers": { "Authorization": "Bearer …" } },
    { "name": "fs", "type": "stdio", "command": "…", "args": [], "env": { "K": "V" } }
  ]
}
```

`sandbox` is present only when `sandboxed` is true. `mounts` are the volumes AoE creates the container with, so a plugin can map container paths back to the host: a linked worktree lists both the worktree and its main repo, and a workspace (multi-repo) session lists the workspace directory plus every main repo outside it. Host paths are sent as they are: not redacted, and not secret. `container_workdir` is the directory the agent runs in, or `null` when AoE cannot determine it. When AoE cannot compute the mounts, `mounts` is empty and the session is still reported as sandboxed.

A server a plugin contributed carries `"plugin": "<plugin id>"`, so a hook can filter by who contributed it. AoE sets it, never a reply: on entries from `mcp.session.servers` (the serving plugin), and on entries an earlier hook added, full definitions and `serve` markers alike (that hook's plugin). A later hook that replaces the entry by name becomes its contributor, and a dropped entry loses it. User, project, agent-native and built-in servers never carry it, so the first hook sees it only on `mcp.session.servers` entries. `plugin` is accepted back on an `add` entry and ignored.

The hook request is built per hook, so every `mcp.rewrite` plugin sees the `mcp.session.servers` entries, not only the serving plugin. Their credential is a `${AOE_PLUGIN_MCP_CRED_<id>}` reference, never the value, and a reference only works in the entry AoE minted it for (see [Serving MCP servers](#serving-mcp-servers-api_version--26)), so a hook cannot obtain a working credential by copying, modifying or re-adding entries. `mcp.rewrite` is still a high-trust grant: a hook can add a stdio command that runs as you, or in the container, and read the agent's environment. This is defense in depth, not a boundary. Entries added with `serve` (below) are likewise never shown to any hook with a credential.

The `result` is an edit; unknown fields are rejected:

```json
{
  "drop": ["fs"],
  "add": [ { "name": "docs", "type": "http", "url": "http://gateway/mcp/docs" } ]
}
```

`drop` is a list of names or `"*"`. `add` entries use the standard `mcpServers` shape plus `name`; an added server replaces any server of the same name, and a later duplicate in `add` wins. Plugins run in ascending `order`, ties broken by plugin id, each seeing the previous result.

A plugin holding both `mcp.rewrite` and `mcp.serve` can also add a server it serves itself, which suits a plugin that decides its servers from the final set:

```json
{
  "add": [ { "name": "docs", "serve": {} }, { "name": "fs", "serve": { "prefix": false } } ]
}
```

- A `serve` entry has only `name` and `serve`, and `serve` only `prefix`; any other field, or any of `type`, `url`, `headers`, `command`, `args` or `env` next to it, makes the reply invalid. So does a `serve` entry from a plugin without both capabilities, or a `name` that is not a valid MCP server name.
- The entry key is `<prefix>__<name>` with `"prefix": true` (the default), as for [served servers](#serving-mcp-servers-api_version--26), and the bare `<name>` with `"prefix": false`. Like any added server it replaces an entry of the same key, without a notification.
- Later hooks see the entry as `{ "name": "<key>", "serve": { "prefix": … }, "plugin": "<plugin id>" }`, without `url` or `headers`, and can drop or replace it by name like any entry. A hook keeps a marker by leaving it alone; echoing it back in `add` is a new `serve` entry of the replying plugin (its `plugin` is ignored), and the reply is invalid if that plugin lacks `mcp.serve`.
- After the last hook, AoE turns each remaining `serve` entry into the same `http` entry a served server gets: `/mcp/plugins/<plugin id>/<name>` with the unprefixed `<name>`, a credential reference for that session and server, and `X-AoE-Session`. No hook ever sees the credential value.
- For a sandboxed session that reaches the daemon over its socket, the entry is the stdio forwarder entry described under [Serving MCP servers](#serving-mcp-servers-api_version--26) instead.
- When the session has no route to the daemon, the entry is omitted together with any entry of the same key, so nothing falls back to the server it replaced, and AoE raises one notification attributed to the plugin naming the cause, unless the launch already did.

A sandboxed session whose hook fails (no worker within the timeout, error reply, invalid reply) is given **no** MCP servers, and AoE raises a notification attributed to the plugin. A sandboxed session with an applicable hook also gets no servers when the plugin host could not start. An `error` reply is reported as a rejection without the worker's text, so a worker cannot write into AoE's logs or notifications through it. Other sessions keep their resolved set.

AoE does not copy host MCP definitions into a Claude sandbox's agent store, though `.mcp.json` files inside the copied Claude `plugins/` directory are copied as they are. Other agents' MCP-bearing config files are still copied into their sandbox stores verbatim, so for those agents the hook alone does not keep host MCP secrets out of the sandbox.

## Serving MCP servers (`api_version >= 26`)

A plugin with the `mcp.serve` capability and a `[runtime]` worker can offer MCP servers to agent sessions without running a process per server: the agent connects to the daemon, and the daemon forwards each request to the worker. No manifest table is needed. The user-facing view is in the [MCP servers guide](guides/mcp-servers.md#plugin-served-servers).

The host sends the worker these requests. All carry host-allocated string ids of the form `h-<n>`; a reply is a JSON-RPC response with the same id, either a `result` or an `error`.

| Method | Params | Result |
|---|---|---|
| `mcp.session.servers` | `{ session_id, agent, sandboxed, profile, cwd, sandbox? }` (same fields as the [`mcp.rewrite`](#hooksmcp_rewrite-api_version--24) request, without `servers`) | `[ { "name": "<server>", "prefix": true } ]` |
| `mcp.tools.list` | `{ server, session_id, cursor? }` | An MCP `ListToolsResult` |
| `mcp.tools.call` | `{ server, session_id, name, arguments }` | An MCP `CallToolResult` |

`session_id` is `null` when the daemon's own token made the call. The host also sends the notification `mcp.cancel` with `{ server, session_id, request_id }` when the agent abandons a call or the call times out; `request_id` is the `h-<n>` id of the request to stop. It needs no reply.

`mcp.session.servers` is asked of every serving plugin at every launch and respawn, and answers which servers that session gets. Each entry has `name` and an optional `prefix` (default `true`); other fields make the reply invalid. A name, and the entry key generated from it (see below), must each be 1 to 128 ASCII characters: a letter or digit first, then letters, digits, `.`, `_` or `-`. An entry that fails either test is silently dropped, without an error or notification, so a plugin cannot tell it was ignored. Entries are de-duplicated by entry key and capped at 64 per plugin (`MCP_SERVE_MAX_PER_PLUGIN`), a limit that also covers the serve entries its rewrite hook adds: those beyond it are dropped. A worker that errors, takes longer than 5 s, or replies with another shape contributes no servers, and AoE raises a notification attributed to the plugin. A JSON-RPC "method not found" error (`-32601`) counts as offering nothing, without a notification, so a plugin that only adds `serve` entries from its rewrite hook need not implement the method. A hung worker therefore delays a launch by up to 5 s.

The agent receives each server under the key `<prefix>__<server>`, where `<prefix>` is the plugin id with `.` replaced by `_`, or under the bare `<server>` with `"prefix": false`. The entry points at `/mcp/plugins/<plugin id>/<server>` on the daemon and carries a credential derived for that session and server, plus an `X-AoE-Session` header; the credential opens only that endpoint and lives as long as its session; it is revoked only when the session is trashed or archived. Being scoped to one plugin server of one live session, it passes the passphrase login on that endpoint from any source, including through `--remote`, `--behind-proxy` or any tunnel, so anyone holding it can reach that endpoint without logging in. Sandboxes need this: Podman with pasta delivers their traffic with the host's address as source, which is not local. The daemon-wide token and plain session credentials still need the login from a non-local source. Policy rules or allowlists written against tool names must use the entry key: agents see tools as `mcp__<key>__<tool>`. Plugin entries replace user-layer entries of the same key, and AoE raises one notification per launch for each plugin that did, naming the replaced keys. The notice fires at launch before the rewrite hooks run, so a hook that later drops or replaces such a key can leave it looking stale. When two plugins produce the same key, an entry under its own plugin's prefix beats an unprefixed one, so no plugin can take another's namespace; otherwise the lowest plugin id keeps it. The other plugin's entry is skipped and AoE notifies that plugin, naming the key. They are not shown by `aoe mcp list` or the dashboard's MCP views.

Credentials stay off the agent's command line where the agent allows it. An agent's MCP entries can end up on its process command line, which every local user can read, so a plugin entry carries the credential only as a reference: `Authorization: Bearer ${AOE_PLUGIN_MCP_CRED_<id>}` for `http`, `AOE_PLUGIN_MCP_TOKEN=${AOE_PLUGIN_MCP_CRED_<id>}` in the forwarder's `env`. `<id>` is a short hash of the credential itself, so it is unique per server and session, stable across respawns, and cannot be computed from public data such as plugin and server names. The value is set in the agent process's environment and the agent expands the reference itself. That environment is readable by you and not by other users, but every child of the agent inherits it: each tool or shell command and every stdio MCP server, in sandboxes too. This is no wider than the old command-line exposure (the same user), but narrower than "only you can read". AoE drops, for every agent, any entry it forwards that mentions an `AOE_PLUGIN_MCP_CRED_` variable (a project, global, profile or native entry, a hook-added entry, or an entry a hook edited) unless it is the exact, unmodified entry AoE minted the reference for, and it delivers a value only when that entry exists. This covers only what AoE forwards. Claude Code also loads `~/.claude.json` and an approved project `.mcp.json` itself and expands `${VAR}` there, and AoE does not filter those files; a reference in such a file can still expand. That path is protected only by the variable name being unguessable to the file's author. The names are visible in the process list (the agent's `--mcp-config` holds the references) and to every `mcp.rewrite` hook. A gateway-style hook that copies a plugin's `Authorization` header into a new key now copies a reference and loses the entry, so that route breaks: use the plugin's own entry instead. This is done only for agents verified to expand `${VAR}` in MCP entries from their own environment, currently Claude Code (`claude` and `claude-code`). Every other agent gets no plugin-served servers: AoE never writes the literal credential into their entries, where it could sit on the agent's command line, and raises one notification per plugin naming the skipped servers. For a sandboxed session the value reaches the container through `docker exec -e <name>`, with the value passed through the runner's environment and never in the `docker exec` argv. `X-AoE-Session` and `AOE_PLUGIN_MCP_SESSION` are not secret and stay literal. If the agent cannot expand a reference it sends the text `${AOE_PLUGIN_MCP_CRED_<id>}` as the bearer, which the daemon rejects.

Forwarded calls have a fixed 600 s ceiling, applied to each call, and the daemon does not throttle or cap concurrent calls per server, so a plugin must bound its own work. The host waits up to 5 s for a worker that is still starting. Results over 4 MiB are rejected. The host always marks results `complete` (rmcp strips it for older clients), so a worker may omit it; any value other than `"complete"` counts as a failure. A failed `mcp.tools.list` surfaces to the agent as a JSON-RPC internal error, a failed `mcp.tools.call` as an MCP error result (`isError: true`). Both texts are fixed: worker error text is never logged or returned, so a worker cannot write into AoE's logs or an agent's context through a failure.

Limits:

- Structured-view (ACP) launches only. Terminal-view sessions get none.
- The endpoints do not depend on `web.mcp_enabled`: the daemon serves them, and creates the session credential secret, when a plugin holding `mcp.serve` is installed at daemon start. The built-in `/mcp` stays behind the setting, and a plain session credential opens nothing while it is off. A serving plugin installed later needs a daemon restart.
- Sandboxed sessions on Linux reach the endpoints through a unix socket, with no bind address, allowed host or network route involved, so any `sandbox.network` value works, `none` included. The daemon serves only the plugin endpoints on `<app dir>/plugin-mcp/mcp.sock` (directory 0700, socket 0600) and accepts only the derived credential there: no daemon token, login session or loopback trust. Every sandbox container mounts that directory read-only at `/aoe/plugin-mcp`, so a daemon restart reaches running containers. Only the daemon that bound the socket writes `forward.mjs` there. The entry is then a stdio server, `node /aoe/plugin-mcp/forward.mjs /aoe/plugin-mcp/mcp.sock <plugin id> <server>` with `AOE_PLUGIN_MCP_TOKEN` (a credential reference, see above) and `AOE_PLUGIN_MCP_SESSION` in `env`, so a rewrite hook sees this form, without the credential value, for sandboxed sessions. The forwarder needs `node` in the image (the default image has it) and a container user that can open a 0600 socket owned by you: container root under rootless Podman or rootless Docker, or root under rootful Docker. At launch AoE runs `test -x` and `test -w` against the mounted directory and socket and `command -v node` as the container's default user; a container that fails any of them gets the HTTP fallback below when it is admitted, else a notification. Verified with rootless Podman and pasta on the default image, including `--network none`.
- Not covered by the socket: macOS, VM-backed runtimes such as Docker Desktop, a Podman machine or Apple Container (the socket does not cross the VM boundary; at launch AoE asks the container for its kernel boot id and treats a mismatch as VM-backed; a container it cannot check, for example one without `cat` or a runtime that does not answer, is treated as unable to use the socket), images whose user is not root (the forwarder then answers every call with a JSON-RPC error), and containers created before this mount existed or under another app directory. Those sandboxes fall back to `http://host.containers.internal:<port>` when the daemon listens on all addresses (a wildcard bind such as `0.0.0.0` or `::`; a specific interface address does not accept connections on the container gateway), `host.containers.internal` is in its allowed hosts (`--allowed-host`) and plaintext remote access is enabled, because the sandbox is a remote peer that the plaintext gate answers with 426 otherwise; Docker on Linux also needs `--add-host host.containers.internal:host-gateway`. Likewise a host session gets a URL on a specific non-loopback bind only with plaintext remote access enabled (a wildcard or loopback bind is reached over loopback).
- Over that fallback, a sandbox's requests come from a shared address (pasta shows the host's LAN address), so its failed attempts count against the same per-IP rate limit budget as every other client on that address.
- A daemon in CityHall mode serves no plugin MCP servers: it refuses the endpoints on every listener, binds no socket, and records no endpoint, so sessions get no entries and the no-endpoint notification below.
- A session with no route gets none of a plugin's servers, and AoE raises one notification per plugin and launch naming the cause: no endpoint, no socket on this platform, a socket that did not start, a container without this daemon's mount (recreate it), a VM-backed runtime, a container user that cannot open the socket, an image without `node`, a container that could not be checked, or a host session whose URL the plaintext gate would reject. The workers are still asked `mcp.session.servers`, so only plugins that offered servers are told.

Any plugin holding `mcp.rewrite` sees the entries from `mcp.session.servers`, with credential references instead of values and `plugin` set. An agent that does not expand references gets none of the plugin-served entries: they are dropped before any hook runs, so no hook sees them. For other agents a hook can replace them by key like any entry. Entries a hook adds with `serve` get their credential only after the last hook. See [Hooks](#hooksmcp_rewrite-api_version--24). If that hook fails for a sandboxed session, the session gets no MCP servers at all, plugin-served ones included.

## Plugin storage

A worker has a host-backed private key/value store, namespaced by its plugin id, that survives daemon and worker restarts (unlike the install directory, which an upgrade can replace). It needs no capability beyond `runtime.worker`, since a plugin can only reach its own namespace.

| Method | Params | Returns |
|---|---|---|
| `plugin.storage.get` | `{ key }` | `{ value }` (null if absent) |
| `plugin.storage.set` | `{ key, value }` | `{}` |
| `plugin.storage.cas` | `{ key, expected, value }` | `{ swapped, current }` |
| `plugin.storage.remove` | `{ key }` | `{ removed }` |

Quotas per plugin: 64 keys, 256-byte keys, 64 KiB values. `cas` (compare-and-swap) enables safe concurrent updates: the write applies only when the stored value equals `expected`.

## Suggested sessions

> **Availability:** the `[suggestions]` manifest block, the `suggestions.*` worker RPCs and the `/api/suggestions` routes are served by this host. The web dashboard and TUI workflows described below arrive with their own changes (#5599, #5600).

For using suggestions from the web dashboard and the TUI, see the [suggested sessions guide](guides/suggested-sessions.md).

A worker declares the criteria it publishes in a `[suggestions]` block:

```toml
[suggestions]
uses_agent = true                # optional; the provider runs an agent itself
[[suggestions.sources]]
criterion = "review_requested"   # stable key, matches each item's `criterion`
label = "Review requests"        # plural, user-facing
description = "Open PRs where your review is requested."
variables = ["pr_number", "repo"]   # `[a-z0-9_]+`, carried by every item
[[suggestions.sources]]
criterion = "ci_failed"
label = "Failing checks on your PRs"
default_enabled = false
```

Plugin [settings](#settings) whose key starts with `suggestion_` also show in Settings › Suggestions, in a "<plugin name> source settings" section unfolded even when `advanced = true`. They remain on the plugin's settings page and save to the same place.

`uses_agent` (default `false`) is informational: Settings shows that the provider spends the user's agent usage, and the host changes nothing. At least one source is required. Criteria are unique, non-empty and at most 256 bytes; labels are non-empty. The block grants no session authority: it needs only `runtime.worker`, not `session.create` or `session.prompt`.

| Method | Params | Result |
|---|---|---|
| `suggestions.sources` | `{}` | `{ sources: [{ criterion, enabled }] }` |
| `suggestions.set` | `{ items }` | `{ ok, accepted, dropped }` |
| `suggestions.error` | `{ message, fix?: { label, method } }` | `{ ok }` |
| `suggestions.checks.set` | `{ checks }` | `{ ok, stored }` |

Call `suggestions.sources` before discovering and skip every criterion that is not enabled. A source is enabled when the user's stored switch says so, else when its `default_enabled` (default `true`) does. The host sends the `suggestions.refresh` notification with `{ refresh_minutes }`, the user's refresh interval (5, 15, 30, 60 or 120; 15 until they choose); on it, discover again and call `suggestions.set` or `suggestions.error`. `refresh_minutes` is a hint for how often to discover on your own: refresh less often if your rate limits need it, never more often. The host re-sends `suggestions.refresh` when the user changes it, but not when a worker starts, so use 15 until the first one arrives.

An item is `{ id, title, reason, source_url, source_ref, preview, project?, mapping_key?, criterion, stale?, variables, draft: { title, prompt } }`. `variables` holds exactly the names the item's source declares. Clients may show `source_ref` as a short human-readable reference, cutting the text before a trailing `#…` first, so send a compact one such as `owner/repo#123`, not a URL; `source_url` is the link. Treat `preview`, `variables` and `draft` as untrusted source content. `projects.list` returns the registered projects so a provider can map its own keys to a `project` name or path. A user's [mapping](#project-mappings) for `mapping_key` takes precedence over `project`.

`suggestions.set` fails with invalid-params, and publishes nothing, when:

- an item names an undeclared `criterion`, or its `variables` differ from the source's declaration;
- `source_url` is not an http(s) URL, or has an empty host, userinfo, percent-encoding in the host, or non-ASCII characters;
- `id`, `title` or `source_ref` is empty, `mapping_key` is present but empty, or an `id` repeats;
- there are more than 500 items, or a string exceeds its bound (`id` and `mapping_key` 256 bytes, `title` and `draft.title` 512, `reason`, `source_url`, `source_ref` and each variable value 2 KiB, `preview` 16 KiB, `draft.prompt` 64 KiB, `project` 4 KiB).

After validation, items of a source that is not enabled are dropped and counted in `dropped`. `stale: true` marks an item whose source closed or changed; clients dim it and offer only Dismiss, and put the explanation in `reason`. Core drops a stale item, counted in `dropped`, at the first publish more than 24 hours after the one that first marked it stale, and keeps dropping it while the provider publishes it stale. Publishing it without `stale` starts it over. `suggestions.error` messages are limited to 4096 bytes.

Each `suggestions.set` replaces the provider's whole set, so an empty `items` clears it and an item left out disappears. `suggestions.error` records the failure on the provider and keeps the last published items; the next successful `suggestions.set` clears it. A user's dismissal is stored by provider and item `id`, survives republishing and never changes the source, so reuse an `id` only for the same suggestion.

### Setup checks

A provider that cannot publish reports why with `suggestions.checks.set`, so the user sees what to fix instead of only a `suggestions.error` message. A check is `{ id, label, status, detail?, fix? }`, where `status` is `ok`, `warn` or `fail`, and `detail` and `fix` are plain display text the host never executes. Core never handles the credentials a check inspects.

Each call replaces the provider's whole list. It fails with invalid-params, and stores nothing, for more than 32 checks, an empty `id` or `label`, a repeated `id`, or a string over its bound (`id` 256 bytes, `label` 512, `detail` and `fix` 2 KiB). When the host sends the `suggestions.checks.run` notification (`{}`), run the checks again and call `suggestions.checks.set`. The host does not send it on its own.

### Suggestions API

Suggestions are stored per profile. The routes below list and act on them; only `POST /api/suggestions/start` and `start-batch` create sessions. Mutations return 403 `read_only` in read-only mode and are refused in CityHall mode; the `GET` routes below stay readable in both, and so does `POST /api/suggestions/prepare`, which mutates nothing (CityHall's "Start with changes" needs it, and `POST /api/sessions` is allowed there). `start` is refused. Item bodies name an item by `{ "provider_id", "id" }`, since an `id` is only unique within a provider.

| Route | Body | Result |
|---|---|---|
| `GET /api/suggestions` | none | `{ profile?, providers: [{ id, name, enabled, status, error?, updated_at?, refreshing, sources: [{ criterion, label, description, variables, enabled, preset, customized }], checks: [{ id, label, status, detail, fix }] }], items, prefs: { homepage, sidebar, refresh_minutes, show_source_ref, provider_order }, setup_offered }`; `setup_offered` says whether the host may offer suggested-session setup |
| `POST /api/suggestions/refresh` | none | `{ ok, notified }` |
| `POST /api/suggestions/checks/run` | none | `{ ok, notified }` |
| `POST /api/suggestions/push` | `{ title, prompt, reason?, project?, id?, pushed_by? }` | `{ ok, id }`; adds or replaces a built-in `builtin.agent` item. Used by `aoe suggest` and the `suggest_session` MCP tool |
| `POST /api/suggestions/dismiss` | `{ provider_id, id }` | `{ ok }` |
| `POST /api/suggestions/undismiss` | `{ provider_id, id }` | `{ ok }` |
| `PATCH /api/suggestions/sources` | `{ provider_id, criterion, enabled }` | `{ provider_id, criterion, enabled }` |
| `PATCH /api/suggestions/prefs` | any of `{ homepage, sidebar, refresh_minutes, show_source_ref, provider_order, provider_enabled }` | `{ ok }` |
| `GET /api/suggestions/presets` | none | `{ presets: [{ provider_id, criterion, customized, preset, defaults }] }`, one per declared source |
| `PUT /api/suggestions/presets` | `{ provider_id, criterion, preset }` | `{ provider_id, criterion, customized: true, preset, defaults }` |
| `DELETE /api/suggestions/presets?provider_id=&criterion=` | none | 204 |
| `PUT /api/suggestions/auto-start` | `{ provider_id, criterion, enabled, cap? }` | `{ provider_id, criterion, auto_start: { cap } \| null }`; `cap` is 1 to 10, default 3; `builtin.agent` is refused with 400 |
| `POST /api/suggestions/start` | `{ provider_id, id, project?: { path }, approve_hooks?, review_fingerprint? }` | 201 `{ session_id, title }` |
| `POST /api/suggestions/start-batch` | `{ items: [{ provider_id, id }] }` (1 to 25) | 200 `{ results: [{ provider_id, id, status, body }] }`; each `status` and `body` is what `start` answers for that item |
| `POST /api/suggestions/fix` | `{ provider_id }` | `{ ok, notified }` |
| `POST /api/suggestions/prepare` | `{ provider_id, id }` | `{ title, initial_prompt, customized, source_ref, project, extra_projects, tool, view, yolo, sandbox, worktree }`; `view` is `structured` or `terminal` |
| `GET /api/suggestions/mappings` | none | `{ mappings: [{ provider_id, mapping_key, projects }] }` |
| `PUT /api/suggestions/mappings` | `{ provider_id, mapping_key, projects }` | `{ provider_id, mapping_key, projects }` |
| `DELETE /api/suggestions/mappings?provider_id=&mapping_key=` | none | `{ ok, deleted }` |
| `GET /api/suggestions/projects` | none | `{ projects: [{ name, path, origin }] }` |

Every route above except the `GET`s takes an optional `expected_profile`: a body field, or a query parameter on the two `DELETE` routes (`refresh` and `checks/run` take it in an optional JSON body; an empty body names none). It names the profile the caller selected its target under. When it is not the profile this Daemon serves, the route answers 409 `profile_mismatch` (also in the `AoE-Error-Code` header) and changes nothing, so a profile switch between selecting and sending cannot land the action on the new profile. Omitted, nothing is checked.

Providers are off until switched on. A provider with no stored switch is listed with `enabled: false`, so a client tells an installed but off provider from no provider (an empty `providers`); its items are not returned and `refresh` and `checks/run` do not notify its worker. `PATCH /api/suggestions/prefs` with `provider_enabled: { "<id>": true }` turns it on, and a switch stored earlier is kept as it was. Ordering a provider does not switch it on. Providers are listed in the user's `provider_order`; a provider never ordered follows, sorted by id. `prefs` defaults to `{ homepage: true, sidebar: false, refresh_minutes: 15, show_source_ref: true, provider_order: [] }` and is per profile. `show_source_ref` makes the web and TUI suggestion rows show each item's `source_ref`. `PATCH /api/suggestions/prefs` is partial: `refresh_minutes` is one of 5, 15, 30, 60 or 120, `provider_order` must list every provider exactly once, and `provider_enabled` maps provider ids to booleans. Other values fail with 400 `validation_failed`, and nothing is stored. `refresh_minutes` is a hint for plugins; core does not schedule refreshes yet. The route is denied in read-only (403 `read_only`) and CityHall mode.

`suggestions.error` may carry a `fix`: a `label` (non-empty, at most 64 bytes) shown on a button beside the error, and a `method` named `suggestions.fix.<name>` (at most 64 bytes, `a-z 0-9 . _ -`). Anything else fails with invalid params and stores nothing. `POST /api/suggestions/fix` sends that method, with `{}`, to the provider's worker as a notification; clients never choose the method, and the route fails with 400 `validation_failed` when the provider has no fix. The worker should open its own fix flow and publish again afterwards. The fix is dropped on the provider's next `suggestions.set` or `suggestions.error`. `GET /api/suggestions` lists `uses_agent: true` and `fix: { label }` (error only) on a provider; the method is never exposed. The route is denied in read-only (403 `read_only`) and CityHall mode.

A provider's `status` is `pending` before its first publish, `ok`, or `error`. Uninstalling a plugin, or updating it to a build whose `[suggestions]` block differs, clears its items, checks and status so it is `pending` again until its next publish; source switches, dismissals, presets, mappings and Session links are kept. Each item is the published item plus `provider_id` and `dismissed`; dismissed items are still returned. Items of a disabled source or provider are hidden, and `refresh` skips a disabled provider. `updated_at` is Unix seconds of the provider's last report. `refresh`, a `sources` change and a `refresh_minutes` change send `suggestions.refresh` to the affected workers.

`refreshing` is true while a refresh the host requested is unanswered, for at most two minutes, and is not stored. Each item has `resolved_project: { name, path }`, or `null`, the registered project a [mapping](#project-mappings) or the unambiguous `project` hint resolves to; group by `path`, never by the provider's `project` text.

`POST /api/sessions` accepts an optional `suggestion: { provider_id, id }`, stored on the session. An item carries `started_session: { id, title, status, archived, created_at, started_via, initial_prompt }` for its primary Session while that Session exists and is not trashed; archiving keeps it. `started_via` is `start` for a Session made by `POST /api/suggestions/start` (the preset's own prompt, agent and project) and `wizard` for one made by `POST /api/sessions` with a `suggestion` ("Start with changes"); a link recorded before the field existed reads as `wizard`. `initial_prompt` is the exact prompt a `start` sent, kept with the link so a preset edited later cannot change it; it is `null` for `wizard`, for a `start` that sent no prompt, and for a link recorded before the field existed. It comes from the [primary link](#suggestions-api) below, so a session that merely carries the `suggestion` but lost the claim does not show. Each item also carries `start: { agent, view, worktree, sandbox, yolo, prompt?, project? }`, what `start` would create with. `yolo` says the Session will auto-approve every tool call, so a client showing what Start creates should show it. `prompt` is the `initial_prompt` it would send (the preset's template expanded as below, else the provider's `draft.prompt`); it is omitted when the expansion is over the limit, where `start` fails. Each source carries its effective `preset` and `customized`. Clients omit `suggestion` to start another session without linking it.

`prepare` resolves settings through the normal repo and profile config. `project` is `{ name, path }` or `null`, resolved in this order: the user's mapping for the item's `mapping_key`, then the item's `project` hint (exactly one registered project by name or exact path), else `null`; an item is never given a scratch project.

A preset is stored per profile and criterion: `{ agent, view: "structured" | "terminal", worktree, sandbox, yolo, prompt }`. Without a stored one (`customized: false`) a kind follows `defaults`: the profile's new-session settings, with the plugin's own prompt (`prompt: null`). `prepare` and `start` take `agent`, `view`, `worktree`, `sandbox` and `yolo` from the preset, and `customized` says whether one is stored. A non-null `prompt` is a template that replaces the provider's `draft.prompt` in `initial_prompt`, nowhere else. `{name}` expands to the item's variable of that name, scanning left to right (`{a{pr_number}}` becomes `{a42}`); unknown names and stray braces stay as typed, and expanded values are never scanned again. An empty or blank prompt gives an empty `initial_prompt`, so a client must then omit it when creating the session, which rejects a blank one. A prompt and its expansion are each limited to 64 KiB; `prepare` and `start` fail with 400 `validation_failed` when the expansion is longer. `PUT` replaces the whole preset and checks that the agent exists. `DELETE` resets the kind and checks neither provider nor criterion, so a preset of a disabled plugin can still be reset, and resetting an absent one succeeds. `GET` and `PUT /presets` list `project_overrides`: the preset fields (`agent`, `view`, `worktree`, `sandbox`, `yolo`) whose default differs in at least one registered Project, because Start resolves defaults per Project (repo config and the Project's worktree and sandbox overrides). `defaults` is the profile's; an editor should mark those fields rather than claim one value.

`POST /api/suggestions/start` creates the Session from the item's kind preset and Project, in the profile the daemon serves. CityHall mode refuses the route. `project` first links the item's repo (its `mapping_key`) to that registered Project: it becomes the primary, the repo's other mapped Projects stay behind it, up to the 16-Project mapping cap (any no longer registered are dropped), and the link stays if the start then fails. Repository hooks are a security gate: when the Project's hooks or MCP servers are not yet approved, nothing is created and the answer is 409 `hooks_need_trust` with `{ commands: { on_create, on_launch, on_destroy }, needs_mcp_trust, mcp_summaries, review_fingerprint }`; `mcp_summaries` are the redacted one-line summaries of the repo's MCP servers. Repeat the call with `approve_hooks: true` and that `review_fingerprint` to approve and start in one step; a configuration that changed since the review is refused with 409 `trust_changed`, and `approve_hooks` without a fingerprint is 400 `validation_failed`. The answer is 201 once the Session exists, like `POST /api/sessions`. A failure answers 422 `start_failed` with `{ reason, hint, retryable }`, and 400 `no_project` (`hint: "link_project"`) when the repo is not linked to a Project. `POST /api/suggestions/start-batch` runs that start for each listed item in order, never approving a hooks review, so one item's refusal (a hooks review, no Project, a failure) never stops the rest; CityHall mode refuses it too.

An item has one primary Session. The first create that carries `suggestion` (`start`, or `POST /api/sessions` from "Start with changes") claims it under the new Session's id, allocated before the create; a second gets 409 `already_started` with `{ session_id }`, the winner's id, which may still be provisioning. `session_id` is omitted when the caller cannot see that Session (CityHall does not see terminal Sessions or one still being created). A claim holds while its start is in flight, however long the create runs. One left by a crash is settled by the next start if its Session was persisted, and taken over if not. A failed create, even one whose client disconnected, frees the claim, so Try again works. Trashing the Session frees it; archiving keeps it. A create without `suggestion` ("Start another") takes no link.

Errors: 404 `unknown_provider` (no active plugin provides suggestions under that id), 409 `stale_suggestion` (the item is no longer published, its source is disabled, or its source closed it; `prepare` and `start` answer all three the same way), 400 `validation_failed` (`criterion` not declared by the provider, an unknown preset agent, or a prompt or its expansion over 64 KiB), 403 `read_only`.

The TUI shows suggestions in Home's preview pane and starts them with `start`, or with `prepare` and the new-session dialog for "Start with changes"; it shows each item's `preview` and `start.prompt` as plain text and sends the snapshot's `profile` as `expected_profile` on each suggestions request it posts. Keys and layout are in the [guide's TUI section](guides/suggested-sessions.md#tui).

### Project mappings

An item may carry an opaque `mapping_key` (a repository, a team). Core stores, per profile, an ordered list of registered project paths for each `(provider_id, mapping_key)`; the first is the primary project and the rest come back as `extra_projects`. A mapping whose primary path no longer names exactly one registered project is ignored and the hint applies instead; stale extras are dropped, and an extra is never promoted. `GET` returns the stored paths as they are, including mappings of providers that are no longer active, and `DELETE` removes them; both stay allowed so stale rows can be repaired (`DELETE` is refused in CityHall mode).

`GET /api/suggestions/projects` lists the registered projects of the suggestions profile, the ones a mapping may name. `origin` is the `owner/repo` of the project's git `origin` remote, case as written, or `null` when the project is not a repository, has no `origin`, or the URL is not a hosted `owner/repo`. The host is not checked. Clients use it to propose mappings; it changes nothing.

`PUT` replaces the list. `projects` holds 1 to 16 distinct registered projects, each named by its path, and is stored as the registered paths. Errors: 404 `unknown_provider`, 400 `validation_failed` (empty or oversized key, bad list, a path that is not exactly one registered project), 403 `read_only`. Mutations are refused in CityHall mode.

## UI slots

Declares the host-rendered slots the worker pushes state into via the `ui.state.set` host RPC, and the plugin's pages.

```toml
[[ui]]
slot = "pane"
id = "my_pane"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `slot` | string | yes | One of the slot names below. Unknown slots are rejected. |
| `id` | string | no | Addressing id for `(slot, id)` state pushes. Required to be non-empty when a command `action` targets it. |
| `title` | string | page only | Page name shown in the dashboard. Required on a `page`. |
| `icon` | string | no | Page only. A [lucide](https://lucide.dev/icons/) kebab-case icon name. |
| `entry` | string | page only | Plugin-relative path to the page's `.html` file. Required on a `page`. See [Pages](#pages). |
| `deep_link` | bool | no | Page only; requires `api_version >= 20`. Opts the page in to receive the `?link=` value of its dashboard URL. See [Deep links](#deep-links). |

| Slot | Scope | Renders |
|---|---|---|
| `status-bar` | global | A segment in the dashboard status bar. |
| `header-start` | global | A chip beside the logo in the dashboard top bar, hidden below the `md` breakpoint and with the wordmark on the compact sidebar rail (requires `api_version >= 17`). |
| `card` | global | A card on the dashboard overview. |
| `sort-key` | global | A named sort option over a `row-column` value. |
| `filter-facet` | global | A named filter over a `row-column` value. |
| `row-badge` | per-session | A badge on the session row. |
| `row-column` | per-session | A text column on the session row. |
| `detail-badge` | per-session | A badge in the session detail view. |
| `pane` | per-session | A dockable tool-window pane (requires `api_version >= 3`). See [Pane payload](#pane-payload). |
| `home-pane` | global | A host-wide docked pane on the dashboard overview and the structured-view pane overlay, carrying the same block vocabulary as `pane` but session-less (requires `api_version >= 13`). Several plugins' home panes stack in snapshot order. |
| `composer-status` | per-session | A read-only badge in the ACP composer footer beside the context hint, web only (requires `api_version >= 25`). |
| `composer-action` | per-session | A button beside the ACP composer controls (requires `api_version >= 8`). |
| `page` | global | A full dashboard view rendered from a plugin HTML file (requires `api_version >= 16`). Optional pushed state is opaque to the host. See [Pages](#pages). |
| `notification` | n/a | A transient notification pushed via `ui.notify`; gated by the `notifications` capability, not a slot declaration. |

### Pages

```toml
[[ui]]
slot = "page"
id = "board"
title = "Board"
icon = "kanban"
entry = "dist/board.html"
```

A page `id` uses only letters, digits, `-` and `_`, and is unique among the plugin's pages. `entry` follows the [screenshot path rules](#screenshots) with an `.html` extension and must resolve inside the plugin directory. A plugin may declare several pages.

The entry must be one self-contained file: inline every script and style, and embed images as `data:` or `blob:` URLs. The host serves only that file, at `GET /api/plugins/{id}/pages/{page_id}` for an active plugin, uncached. Its policy sandboxes the document (`sandbox allow-scripts allow-downloads`), so it gets an opaque origin even when the URL is opened directly, runs the inline script and style and `data:`/`blob:` images, and blocks every other resource load, connection and form submission. As a second layer, the URL refuses any query string and any request whose `Sec-Fetch-Dest` is present and not `iframe`, and only the dashboard may frame it. Browsers send `Sec-Fetch-Dest` only over HTTPS or to localhost, so a dashboard reached over plain HTTP by LAN IP relies on the sandbox alone.

The dashboard opens a page at `/plugins/{id}/{page_id}`, from its button beside the TopBar search or the command palette, in an iframe with `sandbox="allow-scripts allow-downloads"` and nothing else. The page cannot read dashboard storage or cookies, call the API, submit forms, open windows or navigate the dashboard; its only data path is the bridge. It can save a file by clicking an `<a download>` on a `blob:` URL. `allow-downloads` does not require a user gesture, so a page can also start a download from script; the browser's own download protections apply and vary by browser, and Safari can block a download started after an `await`. Start downloads from a click handler. A manifest with `api_version >= 18` can rely on downloads being allowed. A page that reloads itself more than 60 seconds after opening loses its authentication when device binding is on; reopen it from the dashboard instead.

A worker may push state for a page with `ui.state.set` (`slot = "page"`, `id` = the page id, no `session_id`). The payload is any JSON object up to 64 KiB, stored unvalidated; `ui.state.remove` clears it. It appears in `GET /api/plugins/ui-state` as a global entry, `{ "plugin_id", "slot": "page", "id", "payload" }`, which the page reads through `plugins.uiState`.

#### Page bridge

The page opens the bridge by posting `ready` to `parent` with target origin `"*"`, since its origin is opaque, transferring one end of a `MessageChannel` and passing `params: { "nonce" }` set to its URL fragment without the `#`. The dashboard frames each opening with a fresh nonce there, so a document the page navigates to cannot open the bridge in its place. Every later request, reply and push travels over that channel, so nothing reaches a document that replaces the page. The dashboard accepts one `ready` per frame and ignores any other message on the window. Every message carries `aoeBridge: 1`, the bridge version.

A request is `{ "aoeBridge": 1, "id", "method", "params"? }`, where `id` is a number or a string of at most 128 characters and `params` an object. The reply echoes the `id` with either `result` or `error: { "code", "message" }`. The dashboard drops, without replying, any message that is not an object, has another version, or has a malformed `id`, `method` or `params`.

| Method | Params | Requires | Result |
|---|---|---|---|
| `ready` | `{ "nonce" }` | none | `null`, then the `context` and `theme` pushes |
| `sessions.list` | none | `session.read` | The sessions, in the rows a worker's [`sessions.list`](#session-listing) returns |
| `plugins.uiState` | none | `session.read` | The `GET /api/plugins/ui-state` body, including other plugins' entries |
| `action` | `{ "method", "params"?, "sessionId"? }` | not read-only or CityHall | `{ "baselineRevision" }`; the call goes to this plugin's own worker, as a [pane action](#pane-actions) does |
| `openSession` | `{ "id" }` | none | `null`; the dashboard navigates to the session, or shows it below the page, with its side panels, when the user turned on split view |
| `session.archive` | `{ "id" }` | `session.read` and `session.write`, not read-only or CityHall | `null`; archives the session and stops its pane |
| `session.trash` | `{ "id" }` | `session.read` and `session.write`, not read-only or CityHall | `null`; moves the session to the trash and stops its pane |
| `session.contextMenu` | `{ "id", "x", "y", "items"? }` | `session.read` | The picked item's `id`, or `null`. Opens the sidebar's session menu at frame coordinates `x`, `y`, clamped to the frame; every action in it is the user's own and follows the dashboard's modes, and it ignores clicks for 300 ms after opening so a page cannot time one onto an item. `items`, at most 20 `{ "id", "label" }`, are appended to the menu under the page's title. Resolves when the menu closes |

Error codes: `unknown_method`, `invalid_params` (also an `id` that `sessions.list` does not return), `capability_denied` (the plugin does not declare the required capability), `read_only`, `cityhall_mode`, and `failed` (the dashboard's request failed). An `action` result only acknowledges delivery; its effect arrives in `plugins.uiState` once the worker updates its state.

The dashboard pushes `{ "aoeBridge": 1, "event", "data" }` messages, with no `id`:

- `context`, after `ready`, and again whenever `readOnly` or `cityhall` changes while the page is open (for example once the dashboard learns the server is writable): `{ "readOnly", "cityhall", "pluginId", "pageId", "bridgeVersion", "link" }`. `link` is the page's [deep link](#deep-links) value, or `null`; hosts without deep links omit it.
- `theme`, after `ready` and on every theme change: `{ "appearance": "dark" | "light", "cssVars": { "--name": "value" } }`, the dashboard's resolved CSS variables.
- `selection`, after `ready` and whenever it changes: `{ "sessionId" }`, the session split view shows below the page, or `null`.
- `link`, whenever the deep link changes while the page is open: `{ "link" }`, the new value or `null`.

When the frame loads a second document, because the page reloaded or navigated itself, the dashboard removes the frame and offers to reopen it. A minimal client:

```ts
let next = 0;
const pending = new Map<number, (reply: { result?: unknown; error?: { code: string; message: string } }) => void>();
const handlers: Record<string, (data: any) => void> = {};
const { port1, port2 } = new MessageChannel();

port1.onmessage = ({ data: m }) => {
  if (m.event) return handlers[m.event]?.(m.data);
  pending.get(m.id)?.(m);
  pending.delete(m.id);
};

function request<T>(msg: object, send: (msg: object) => void): Promise<T> {
  const id = ++next;
  const reply = new Promise<T>((resolve, reject) =>
    pending.set(id, (m) => (m.error ? reject(m.error) : resolve(m.result as T))),
  );
  send({ aoeBridge: 1, id, ...msg });
  return reply;
}

const call = <T>(method: string, params?: object) => request<T>({ method, params }, (m) => port1.postMessage(m));

handlers.theme = ({ cssVars }) => {
  for (const [k, v] of Object.entries(cssVars)) document.documentElement.style.setProperty(k, String(v));
};
handlers.context = (ctx) => console.log("bridge context", ctx);
request({ method: "ready", params: { nonce: location.hash.slice(1) } }, (m) => parent.postMessage(m, "*", [port2]))
  .then(() => call<{ id: string }[]>("sessions.list"))
  .then((sessions) => console.log(sessions));
```

#### Deep links

A page with `deep_link = true` can be opened on one item: `/plugins/{id}/{page_id}?link={value}`. The dashboard, not the page URL, takes the value: the frame URL still refuses a query string, and the value reaches the page only as `context.link` after `ready` and as later `link` pushes. `value` is 1 to 128 characters of `A-Z a-z 0-9 - _ . ~`, so it never needs escaping; anything else, or a page without `deep_link`, yields `null`.

The value is opaque to the host and chosen by the plugin, for example an item id. Treat it as untrusted input: look the item up yourself, and show the plain page for an unknown value. It never grants anything, and the dashboard's login is unchanged: a user who is not signed in sees the sign-in screen at the same URL, then the page.

With `api_version >= 20`, `ui.notify` takes an optional `link: { "page", "value" }` naming one of the plugin's own pages with `deep_link = true`; the host builds the URL above, so clicking the toast in the web dashboard opens the page on that item. The toast keeps its `tone`, and the TUI shows it without a link. A page that is undeclared or lacks `deep_link`, or a malformed value, is rejected with `invalid_params`. For a message sent outside aoe, use the absolute URL on the dashboard's own origin.

### Badge payload

`status-bar`, `header-start`, `row-badge`, `detail-badge` and `composer-status` take either one badge, `{ text?, icon?, tone?, href?, tooltip? }` (`status-bar`, `header-start`, `detail-badge` and `composer-status` require `text` or `items`), or an `items` list of such badges that replaces the top-level fields. `items: []` clears the badge. Row badges also accept `presentation?` on the badge and its items, as described below.

`status-bar`, `header-start`, `detail-badge` and `composer-status` also take an integer `order`, default 0, lowest first; ties keep plugin id then entry id order. In the terminal UI they render as text and tone in the status line, `header-start` first; icons, tooltips and links are web only. `composer-status` has no terminal rendering.

`row-badge` items on terminal session rows render items with `text` or `tooltip`, using `tooltip` when `text` is absent. With no `items` list, the top-level `row-badge` uses `tooltip` as a fallback label for missing `text`. `row-column` renders text only; the TUI omits icons and links. The status strip is right-aligned. In the local session list, it appears before the activity age when enabled, and its bounded lane considers one item per plugin before considering a second item from any plugin. Items may be truncated or omitted when space runs out.

For a row badge or its individual items, set `presentation` to `"glyph"` when the TUI `text` is a glyph. The default is `"text"`; both values are accepted explicitly. A glyph uses one separator cell and text uses two. Tooltip fallback always uses text spacing. The `icon` field is web-only and does not determine terminal spacing. In remote home, the gap between the status strip and project path remains two cells.

In the web dashboard, give items a shared `group` to collapse them into one chip that shows one item at a time. Clicking or tapping it advances to the next item and wraps around, for example a usage badge cycling `5h`, `7d` and `opus` values:

```json
{ "items": [
  { "text": "5h 40%", "group": "usage" },
  { "text": "7d 12%", "group": "usage" },
  { "text": "stale", "tone": "warn" }
] }
```

Items without a `group` stay separate chips, and each distinct `group` cycles independently. The position is kept per browser tab and is never sent to the worker. A cycling chip ignores `href`; a group with a single item renders as a normal chip. The TUI cannot click and shows the first item with text.

Grouped `row-badge` items remain separate in the TUI and do not cycle.

### Pane payload

A `pane` entry renders a dockable tool-window, pushed with `ui.state.set`:

```json
{
  "title": "GitHub",
  "default_location": "right",
  "icon": "git-branch",
  "blocks": [{ "kind": "heading", "text": "GitHub" }],
  "footer": { "text": "refreshed 12:07", "value": "blocked", "tone": "danger", "icon": "refresh-cw" }
}
```

| Key | Type | Notes |
|---|---|---|
| `title` | string | Shown on the dock tab. |
| `body` | string | The simple form: plain text, used only when `blocks` is absent. |
| `blocks` | array | The block list (below). Takes precedence over `body`. |
| `default_location` | string | `right` or `bottom`. The dock it first opens in; the user can move it after. |
| `icon` | string | Lucide name for the pane-menu and dock-tab icon. A manifest `icon_asset` outranks it. |
| `footer` | table | A status line pinned below the scrolling block list: `text` left, tone-colored `value` right, plus an optional `icon`. Requires `api_version >= 12`. |

The payload is capped at 64 KiB. Everything but `blocks` is validated strictly; `blocks` is opaque JSON, and **each surface renders the kinds it knows and drops the rest.** That forward-compatibility contract cuts both ways: a new kind needs no host change, but an older host renders nothing for it, so a pane that depends on a newer kind should say so with `api_version` and `aoe_version`.

#### Block kinds

| `kind` | Required | Optional |
|---|---|---|
| `heading` | `text` | |
| `note` | `text` | `tone` |
| `markdown` | `text` | `tone` (requires `api_version >= 15`) |
| `divider` | | |
| `row` | one of `label` / `value` / `prefix` / `icon` / `avatar` | `sublabel`, `tone`, `value_tone`, `color`, `href`, `tooltip`, `mono`, `selected`, `badges`, `method`, `params` |
| `section` | | `title`, `children`, `value`, `value_tone`, `badges`, `icon`, `tone`, `boxed`, `scroll`, `collapsible`, `collapsed` |
| `callout` | one of `title` / `detail` | `icon`, `tone`, `color`, `actions` |
| `bar` | `segments` | `caption` |
| `sparkline` | `values` | `max`, `tone`, `bands`, `caption` (requires `api_version >= 13`) |
| `columns` | `children` | |
| `action` | `label`, plus one of `method` / `href` / `disabled` | `icon`, `tone`, `tooltip`, `variant` |
| `comment` | one of `author` / `body` | `path`, `line`, `resolved`, `href` |

`tone` is one of `neutral` / `info` / `success` / `warn` / `danger`. `color` is a validated `#rgb` / `#rrggbb` literal for a hue no tone names (a merged PR's purple); anything else is ignored.

An `href` renders as a link only when it is an `http(s)` URL or a path starting with a single `/` and containing no backslash, tab or line break. A link to a dashboard route navigates in place; any other link opens in a new tab, including a path that normalizes to `//host` such as `/..//evil.com`, which opens on the dashboard's own origin.

**`row`** lays out at most two lines: `prefix` (mono, tone-tinted) and `label` lead the first with `value` pinned right; `sublabel` leads the second with `badges` (`{ text?, icon?, tone?, tooltip? }`) pinned right. `value_tone` colors the trailing token independently of the row, and `mono` monospaces the row's text. A `method` makes the row body a button firing that worker method, and an `href` alongside it becomes a separate trailing link-out; with `href` alone the whole row is the link. `selected` marks the row as the pane's current subject.

**`section`** groups `children`, with a right-pinned `value` summary or `badges` in its header. `boxed` draws a bordered card, `scroll` caps the body height so a long list scrolls inside the section, and `collapsible` folds it via a native `<details>` (`collapsed` sets the initial state).

**`callout`** is a tone-bordered verdict card: glyph, `title`, `detail` paragraph, and full-width `actions`. Use it for the one thing the pane is telling the user, and a `section` for a list.

**`bar`** stacks `segments` (`{ value, tone?, color?, label? }`) proportionally; segments without a positive `value` are dropped and a bar left with nothing renders nothing. **`sparkline`** plots `values` (oldest first) as a history line, with `max` fixing the top of the scale so a series does not auto-scale each refresh and `bands` (`{ at, tone }` thresholds) recoloring each sample by the highest band it reaches. Both take a `caption` beneath.

**`markdown`** renders `text` as GitHub-flavoured markdown: headings, emphasis, inline and fenced code, lists including task lists, blockquotes, links and tables. `text` must be a string, or the host rejects the push; it has no cap of its own beyond the pane payload cap. Plugin text is untrusted, so the web dashboard sanitises it: raw HTML, HTML comments and `<details>` tags are dropped (markdown inside them still renders when set off by blank lines, as on GitHub; without them the whole HTML block is dropped), an image shows its alt text and never loads its URL, and a link follows the `href` policy above (a link that fails it shows as plain text). The TUI cannot render rich markdown and shows `text` as plain lines with the markers kept.

**`columns`** lays its `children` out in equal fractions, and a single child spans the full width, so eliding one card collapses the row cleanly.

**`action`** forwards `method` to the worker (see [Pane actions](#pane-actions)); with `href` and no `method` it is a link-out button. `disabled` renders it inert and non-navigating, and `variant: "primary"` gives the brand-filled treatment.

#### Pane actions

Clicking an `action` block, or a `row` carrying a `method`, POSTs to `/api/plugins/{id}/action` with `{ method, params, session_id }`. `params` is the block's own `params` object, forwarded as given apart from the host-owned `session_id` and `origin`. The host also removes `composer` unless the plugin declares the composer-read capability. One method can serve every row in a list:

```json
{ "kind": "row", "label": "warn when daemon is stale", "prefix": "#3231",
  "method": "github.select_pr", "params": { "pr": "o/r#3231" } }
```

The host merges in the authoritative `session_id` (a plugin cannot spoof it) and delivers the call as a **fire-and-forget JSON-RPC notification**: no reply, no return value. The worker does its work and re-pushes its UI state, and the clicked control spins until the plugin's UI revision moves, with a 15s timeout. Actions are read-write-mode only and are not passphrase gated, so treat every method as reachable by anyone who can use the dashboard.

The host also sets `params.origin`, overwriting any client value. It is session provenance, not proof of human intent:

- `dashboard`: the request carried a signed-in browser login. Programmatic callers holding that session can send it too.
- `socket`: a process on the daemon's Unix socket (same user, such as a coding agent).
- `token`: any other caller (access token, or `--auth none`).

A plugin that needs a person to approve something must not rely on `origin` alone; it needs its own trusted confirmation step. `params` must be a JSON object, `null` or omitted; anything else gets `400`.

The TUI renders panes read-only: it draws the text of every kind (dropping icons, hrefs, and tooltips, and stacking `columns`) but cannot fire an action, so `action` blocks appear as inert `[action] <label>` labels.

#### Link targets

An `href` is either an absolute `http(s)://` URL, or a same-origin path
resolved against the host's own dashboard origin:

- **In the web dashboard** a relative path navigates inside the open SPA, so
  the browser stays on the same origin and no page reload is needed.
- **In the TUI** there is no browser origin, so a relative path needs a
  reachable dashboard to open: a local `aoe serve` web daemon (its published
  URL, including its bootstrap token, is used), or an explicit
  `AOE_DASHBOARD_URL` when the TUI is attached to a remote daemon
  (`AOE_DAEMON_URL`). A core-only daemon has no TCP listener and no dashboard,
  so a relative link is refused with a message rather than opened against a
  guessed host. Absolute `http(s)` hrefs work everywhere and need no
  dashboard.

Dashboard bootstrap query parameters are preserved across the navigation, so
a tokenized `serve.url` still authenticates a relative link.

### Composer action payload

A `composer-action` entry renders a host-owned button in the dashboard's ACP composer, pushed with `ui.state.set`. `label` and `method` are required; `icon`, `tooltip`, `tone`, and `disabled` are optional.

```json
{ "label": "Dictate", "method": "dictation.start", "icon": "mic" }
```

On click the dashboard POSTs `method` to `/api/plugins/{id}/action` with the active `session_id`. With `composer.read` the forwarded params also carry `{ "composer": { "text", "selection_start", "selection_end" } }`, a click-scoped snapshot of the draft; without it the server strips that snapshot before forwarding.

To mutate the draft, include a `draft_operation` in the pushed payload, which requires `composer.write`:

```json
{
  "label": "Dictate",
  "method": "dictation.start",
  "draft_operation": { "kind": "insert-text", "id": "transcript-1", "text": "Hello." }
}
```

`kind` is `insert-text`, `replace-selection`, or `set-text`. `id` must be stable and non-empty: the dashboard applies each operation id once, so a persistent UI-state entry cannot replay the edit on every poll.

## Status

Reserved. The manifest accepts `[[status]]` (with `api_version >= 4`) but no host surface reads it yet; use a `status-bar` [UI slot](#ui-slots) for visible status.

```toml
[[status]]
id = "pr_state"
label = "PR state"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Stable segment id. |
| `label` | string | no | Human-readable text. |

## Themes

```toml
[[themes]]
name = "My Theme"
path = "themes/my-theme.toml"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Theme name in the picker. Must not collide with a builtin. |
| `path` | string | yes | Theme TOML path, relative to the plugin directory. |

## Screenshots

Up to 8 marketplace screenshots, shown in the plugin detail view. Requires `api_version >= 5`.

```toml
[[screenshots]]
path = "assets/screenshots/overview.png"
alt = "The plugin's pane showing live status."
caption = "Live status in the pane."
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | Repository-relative image path. No URL scheme, no leading separator, no `..`; must be PNG, JPEG, GIF, or WebP. |
| `alt` | string | yes | Accessible description; non-empty. |
| `caption` | string | no | Caption shown beneath the image. |

## Runtime

The worker the host spawns and supervises, in one of two kinds. Omit it for a static, metadata-only plugin.

Both runtime kinds require `runtime.worker` in the manifest's top-level `capabilities`. Missing declarations are rejected before installation or build execution, even with `--yes`. Installed manifests missing the declaration are skipped with a load diagnostic after the daemon is upgraded and restarted. Correct the declaration and approve the changed manifest; an old grant does not authorize new manifest bytes.

### Command

The host runs the build steps at install or update, then launches `command`.

```toml
capabilities = ["runtime.worker"]

[runtime]
kind = "command"
command = [".aoe-build/venv/bin/my-plugin-worker"]

[[runtime.build]]
command = ["python3", "-m", "venv", ".aoe-build/venv"]
platforms = ["linux", "macos"]
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `command` | array of string | yes | argv. Plugin-relative by default (must contain a path separator, never absolute) so the daemon's `PATH` never decides whether the worker launches. With `system = true` it must instead be a bare program name resolved on `PATH`. |
| `system` | bool | no | Resolve `command[0]` on the host `PATH` (for genuine system tools only). Defaults to `false`. |
| `build` | array | no | Ordered build steps, run once at install or update inside the plugin directory, in the user's interactive shell. |

Build into `.aoe-build/` (the host's build-output directory); the host excludes it from the plugin tree hash, so a venv, `node_modules`, or `target/` there does not break integrity verification.

#### Build step

| Key | Type | Required | Notes |
|---|---|---|---|
| `command` | array of string | yes | argv, same resolution policy as the launch `command`. |
| `platforms` | array of string | no | Restrict to OS names: `linux`, `macos`, `windows`. Empty runs on all. |

### Release binary

The host downloads a release asset instead of building from source.

```toml
capabilities = ["runtime.worker"]

[runtime]
kind = "release-binary"
asset = "my-plugin-${target}.tar.gz"
bin = "my-plugin-worker"
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `asset` | string | yes | Asset-name template; `${os}`, `${arch}`, `${target}` are substituted before matching the release. |
| `bin` | string | no | Executable path inside the extracted archive. Omit to run the downloaded asset directly (a raw, non-archive binary). |

### Worker logs

The worker's stdout carries JSON-RPC only; write diagnostics to stderr. The host captures stderr in `<app_dir>/plugin-workers/<plugin-id>_<worker-uuid>.log`, one file per start, and `aoe plugin logs <id>` prints the newest path. Only the five newest logs per plugin are kept. See [Worker logs](plugins.md#worker-logs).

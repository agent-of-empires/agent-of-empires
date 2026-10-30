# Repository Configuration & Hooks

A repo can carry `.agent-of-empires/config.toml` so team members share project defaults and lifecycle hooks. Run `aoe init` for a commented template and commit it. The legacy `.aoe/config.toml` path is still read; rename it with `mv .aoe .agent-of-empires`. If both exist, the new path wins.

The [hook trust system](#hook-trust-system) requires each developer to approve commands before they run.

## Hooks

```toml
[hooks]
on_create  = ["npm install", "cp .env.example .env"]
on_launch  = "npm install"                   # a single command may be a string
on_destroy = ["docker-compose down"]
```

- **`on_create`** runs once, when the session is first created. A failing command aborts creation, so use it for one-time setup.
- **`on_launch`** runs on every start, including restarts. Failures are logged as warnings and do not prevent a user-initiated start, though during startup recovery a timed-out hook marks the recovered session as errored rather than launching with partial setup.

Native starts release their API locks while pre-launch hooks run, so hooks can call the daemon. The session keeps its lifecycle reservation: a competing start or stop returns a conflict rather than taking ownership. The launcher revalidates that reservation after the hook, before spawning the agent.

- **`on_destroy`** runs before worktree and sandbox cleanup, while their resources remain available. Failures warn without preventing deletion.
For a host-side equivalent that always runs outside the container, see [`host_hooks.before_destroy`](configuration.md#host-hooks).

Hooks run inside the container for a sandboxed session and in your host shell otherwise, so a path can resolve in one mode and not the other. An absolute host path fails in the container unless `sandbox.extra_volumes` mounts it, and a repo cannot set that key. Guard optional scripts on their presence only, so a real failure still aborts creation:

```toml
[hooks]
on_create = ["sh -c '[ -x /opt/setup.sh ] || exit 0; exec /opt/setup.sh'"]
```

Keep environment-specific hooks out of global config. A global `on_create` applies to every repo that does not declare its own, so one unresolvable path there blocks session creation in projects that never mention it. When `on_create` fails, the TUI and `aoe add` name the config file that declared it; the web dashboard shows a generic error and logs the file to the `aoe serve` log.

#### Available environment variables

Lifecycle hooks receive `AOE_SESSION_ID`, `AOE_SESSION_TITLE`, `AOE_PROJECT_PATH` (equals `$PWD` in `on_create` and `on_launch`), `AOE_PROFILE`, `AOE_TOOL`, `AOE_GROUP_PATH`, and `AOE_SESSION_BRANCH` for worktrees. Container hooks receive the same variables. Quote expansions that may contain spaces:

```toml
[hooks]
on_create  = ["port \"$AOE_SESSION_TITLE\""]
on_destroy = ["port rm \"$AOE_SESSION_TITLE\""]
```

[Status-transition hooks](configuration.md#status-hooks) are global/profile-only and add `AOE_OLD_STATUS`, `AOE_NEW_STATUS`, and `AOE_STATUS_CHANGED_AT`.

## What a repo may override

A repo config is code you did not write, so the keys that decide what AoE launches, or how much it is allowed to do, are ignored from it (with a warning naming them). Set those in your global or profile config instead.

| Section | A repo may set | A repo may not set |
|---|---|---|
| `[hooks]` | everything, behind the trust prompt | |
| `[session]` | `agent_detect_as` | `custom_agents`, `default_tool`, `agent_command_override`, `agent_extra_args`, `agent_acp_cmd`, `yolo_mode_default`, and the rest |
| `[sandbox]` | `volume_ignores`, `port_mappings`, `cpu_limit`, `memory_limit`, `auto_cleanup`, `default_terminal_mode` | `enabled_by_default`, `default_image`, `container_runtime`, `environment`, `extra_volumes`, `mount_ssh`, `selinux_relabel`, `privileged`, `cap_add`, `cap_drop`, `security_opt`, `extra_run_args` |
| `[worktree]` | `auto_cleanup`, `delete_branch_on_cleanup` | `enabled`, `path_template`, `bare_repo_path_template`, `workspace_path_template` |

`[tmux]`, `[sound]`, `[updates]`, and `[diff]` are personal settings and are not read from a repo at all. `sandbox.environment` is denied because its bare `KEY` and `KEY=$VAR` forms copy host variables into the container, and `default_tool` because session launch prefers an exact `custom_agents` match, which would let a repo select a user-defined host command. `container_runtime` is global-only everywhere.

List fields such as `volume_ignores` and `port_mappings` accept either an array or a single string.

```toml
[hooks]
on_create = ["npm install", "npx prisma generate"]

[session]
agent_detect_as = { my-agent = "claude" }

[sandbox]
volume_ignores = ["node_modules", ".next"]

[worktree]
auto_cleanup = true
```

## Hook trust system

AoE prompts before running repo hooks and stores trust decisions globally, shared across profiles. Changed commands require approval again. The same gate covers [project-local MCP servers](mcp-servers.md#project-local-servers-need-repo-trust).

`aoe add --trust-hooks .` skips this gate for CI or repos you control. It does not grant [agent hook approval](configuration.md#agent-hook-approval), which covers hooks AoE writes into the agent's own config.

Repo values override global and profile values field by field; unset fields inherit. See [Configuration](configuration.md) for precedence and the full reference.

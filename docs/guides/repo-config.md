# Repository Configuration & Hooks

A repo can carry its own `.agent-of-empires/config.toml`, so every team member using AoE on it gets the same project defaults and lifecycle hooks. `aoe init` writes a commented template. The legacy `.aoe/config.toml` path is still read, but rename it (`mv .aoe .agent-of-empires`); if both exist, the new one wins.

The file is meant to be committed. The [hook trust](#hook-trust) system is what makes that safe: each developer approves the commands before they run.

## Hooks

```toml
[hooks]
on_create  = ["npm install", "cp .env.example .env"]
on_launch  = "npm install"                   # a single command may be a string
on_destroy = ["docker-compose down"]
```

- **`on_create`** runs once, when the session is first created. A failing command aborts creation, so use it for one-time setup.
- **`on_launch`** runs on every start, including restarts. Failures are logged as warnings and do not prevent a user-initiated start, though during startup recovery a timed-out hook marks the recovered session as errored rather than launching with partial setup.
- **`on_destroy`** runs when a session is deleted, before worktree and sandbox cleanup, so teardown commands can still reach running containers. Failures never prevent deletion.

Hooks run inside the container for a sandboxed session and in your host shell otherwise, so a path can resolve in one mode and not the other. An absolute host path fails in the container unless `sandbox.extra_volumes` mounts it, and a repo cannot set that key. Guard optional scripts on their presence only, so a real failure still aborts creation:

```toml
[hooks]
on_create = ["sh -c '[ -x /opt/setup.sh ] || exit 0; exec /opt/setup.sh'"]
```

Keep environment-specific hooks out of global config. A global `on_create` applies to every repo that does not declare its own, so one unresolvable path there blocks session creation in projects that never mention it. When `on_create` fails, the TUI and `aoe add` name the config file that declared it; the web dashboard shows a generic error and logs the file to the `aoe serve` log.

Each hook receives the session's metadata as environment variables: `AOE_SESSION_ID`, `AOE_SESSION_TITLE` (also the worktree branch name), `AOE_PROJECT_PATH` (equals `$PWD` in `on_create` and `on_launch`), `AOE_PROFILE`, `AOE_TOOL`, `AOE_GROUP_PATH`, and `AOE_SESSION_BRANCH` on worktree sessions. Container hooks get the same set. Quote any expansion that may contain spaces, since titles often do:

```toml
[hooks]
on_create  = ["port \"$AOE_SESSION_TITLE\""]
on_destroy = ["port rm \"$AOE_SESSION_TITLE\""]
```

Status-transition hooks are configured separately, in [`[status_hooks]`](configuration.md#status-hooks), and are global or profile only.

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

## Hook trust

The first time AoE sees hooks in a repo it prompts you to review and approve them, so an untrusted repo cannot run arbitrary commands. Trust decisions are stored globally, shared across profiles, and keyed to the commands themselves, so a change to `.agent-of-empires/config.toml` re-prompts. The same gate covers a repo's [project-local MCP servers](mcp-servers.md#project-local-servers-need-repo-trust).

`aoe add --trust-hooks .` skips the prompt, for CI or repos you control.

Repo values are the last layer of the [configuration precedence](configuration.md), overriding global and profile values field by field.

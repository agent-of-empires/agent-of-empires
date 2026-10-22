# Container Sandbox

A sandboxed session runs its agent in a private container with the project mounted at `/workspace`. Agent credentials are seeded into a private per-session store; containers do not share writable agent history. Claude Code shares only its credential file; see [Shared credentials](#shared-credentials).

Docker is the default. Podman and Apple Container are alternatives.

## Creating one

```bash
aoe add --sandbox .
aoe add --sandbox-image myregistry/custom:v1 .
aoe add --sandbox -l .
aoe remove <session>
aoe remove <session> --keep-container
```

In the TUI or web wizard, enable the sandbox toggle when creating a session. The TUI shows it when a container runtime is available and cannot set a per-session image.

## Deleting a stopped sandbox

AoE does not start a stopped managed container to remove its worktree. A
host-writable worktree can be removed without starting the container. If
root-owned files block removal, purge reports the container and worktree path
and retains both for retry.

Verify that exact worktree and repair the blocking ownership or permissions
with elevated host permissions, then retry the same purge. Do not start the
managed entrypoint merely to clean up files. Do not remove ownership journals
or unrelated agent stores to bypass the refusal.

## Configuration

```toml
[sandbox]
enabled_by_default = false
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest"
auto_cleanup = true
cpu_limit = "4"
memory_limit = "8g"
environment = ["ANTHROPIC_API_KEY"]
```

| Option | Default | Description |
|--------|---------|-------------|
| `enabled_by_default` | `false` | Auto-enable the sandbox for new sessions. Overridable per project. |
| `default_image` | `aoe-sandbox:latest` | Image to run |
| `container_runtime` | `docker` | `docker`, `podman`, or `apple_container`. Global only; a profile or repo override is ignored |
| `auto_cleanup` | `true` | Remove the container when the session is deleted |
| `cpu_limit` / `memory_limit` | (none) | Per-container limits, e.g. `"4"` and `"8g"` |
| `environment` | `[]` | Env vars for the container (`KEY` or `KEY=VALUE`, see below) |
| `volume_ignores` | `[]` | Paths excluded from the project mount (see [Volume ignores](#volume-ignores)) |
| `volume_ignores_strategy` | `"anonymous"` | `"anonymous"` or `"named"` (see [Volume ignores](#volume-ignores)) |
| `extra_volumes` | `[]` | Additional volume mounts |
| `port_mappings` | `[]` | Published ports, e.g. `"3000:3000"` |
| `mount_ssh` | `false` | Mount `~/.ssh/` read-only |
| `selinux_relabel` | `false` | Append `:z` to every bind mount (Docker and Podman only) |
| `default_terminal_mode` | `"host"` | Where the paired terminal runs: `"host"` or `"container"` |
| `privileged` | `false` | Run privileged (`--privileged`) |
| `cap_add` / `cap_drop` | `[]` | Capabilities added or dropped |
| `security_opt` | `[]` | Security options, e.g. `seccomp=unconfined` |
| `extra_run_args` | `[]` | Extra arguments passed to container run, before the image |

Docker and Podman honor every run-policy option. Apple Container honors `cap_add` / `cap_drop` and ignores `privileged` and `security_opt` with a warning; `extra_run_args` is passed through everywhere.

## Volume mounts

### Automatic mounts

| Host path | Container path | Mode |
|-----------|----------------|------|
| Project directory | `/workspace` | RW |
| `~/.gitconfig` | `/root/.gitconfig` | RO |
| `~/.ssh/` (with `mount_ssh`) | `/root/.ssh/` | RO |
| The session's artifact dir | `/aoe/artifacts` | RW |
| `<agent config>/sandbox-v2/<instance-id>/` | the agent's config path, e.g. `/root/.claude/` | RW |
| `~/.claude/sandbox-v2/.credentials.json` | `/root/.claude/.credentials.json` | RW |

### Volume ignores

`volume_ignores` accepts literal paths relative to each workspace root, or glob patterns (`*`, `?`, `[` or `]`) expanded when the session is created. Literal mounts shadow paths even if they do not exist yet.

```toml
[sandbox]
volume_ignores = ["node_modules", "target", "**/bin", "**/obj"]
```

A glob is a point-in-time snapshot: directories created later inside the container are not shadowed. List known paths literally or recreate the session for new matches. The TUI and web dashboard confirm this once before sandbox creation.

### Volume ignores strategy

The default `"anonymous"` strategy can expose ignored host directories under
macOS Docker Desktop's VirtioFS. Use `volume_ignores_strategy = "named"` for
deterministic Docker/Podman volumes stored inside the VM. Apple Container falls
back to anonymous volumes with a warning.

Named volumes follow their mount paths. Moving a worktree starts fresh caches;
the next session start removes the volumes left at its old paths. Other
orphaned volumes remain untouched. Inspect `docker volume ls -q --filter
name=aoe-vi-` and remove only volumes you recognize.

Session deletion reclaims named volumes only after their container is removed
or confirmed absent. A failed container removal retains them for retry.

## Environment Variables

Each `environment` entry is a bare `KEY` (pass the host value through) or `KEY=VALUE`. A value starting with `$` reads a host variable (`$$` escapes a literal `$`), so secrets can live in your shell profile rather than in `config.toml`; an unset variable is skipped. Keys must match `[A-Za-z_][A-Za-z0-9_]*`; anything else is dropped with a warning.

```toml
[sandbox]
environment = [
    "ANTHROPIC_API_KEY",              # pass through from the host
    "GH_TOKEN=$AOE_GH_TOKEN",         # read AOE_GH_TOKEN, inject as GH_TOKEN
    "CUSTOM_API_KEY=sk-sandbox-key",  # literal value
]
```

To mint a short-lived value on the host at launch instead, use [`host_hooks.before_start`](configuration.md#host-hooks). Host (non-sandboxed) sessions read the top-level [`environment`](configuration.md#host-environment) list instead; the two are disjoint.

## Folder trust

Claude Code, Codex and Gemini need a trusted directory before launch. AoE pre-trusts the staged container config for Claude Code in every sandbox, and for Codex and Gemini only in YOLO mode.

Trust activates the repo's `.claude/settings.json`, including hooks and permission rules; `.mcp.json` still asks per server. For host sessions, see `session.pre_trust_agent_folders` in [Configuration](configuration.md#session).

Custom wrappers must keep the managed container config path and AoE's config-dir variables. `session.agent_config_dir` names the host root; each sandbox gets a private child. Do not shadow its managed mount with `sandbox.extra_volumes`; AoE warns about overlapping source paths during preparation. Host-only account selection stays outside the sandbox.

## Available images

Two official images are available:

| Image | Contents |
|---|---|
| `ghcr.io/agent-of-empires/aoe-sandbox:latest` | Supported agent CLIs, git, ripgrep and fzf |
| `ghcr.io/agent-of-empires/aoe-dev-sandbox:latest` | Base image plus Rust, uv, Node.js LTS and GitHub CLI |

Use the dev image per session with `aoe add --sandbox-image ghcr.io/agent-of-empires/aoe-dev-sandbox:latest .`, or set `[sandbox] default_image`.

## Custom Docker Images

Extend either image for project dependencies:

```dockerfile
FROM ghcr.io/agent-of-empires/aoe-sandbox:latest
RUN apt-get update && apt-get install -y python3 python3-pip \
    && rm -rf /var/lib/apt/lists/*
```

Build and select it:

```bash
docker build -t my-sandbox:latest .
aoe add --sandbox-image my-sandbox:latest .
```

Alternatively set `[sandbox] default_image = "my-sandbox:latest"`; push it to a registry if another host needs it. A custom image used with structured view must include the agent's ACP adapter or the handshake fails.

For a custom agent whose wrapper points the CLI at another directory, name that host root in `session.agent_config_dir` and declare its native CLI with `session.agent_execution_as`; `agent_detect_as` selects status and ACP adapter behavior, not native-store ownership. A foreign declared root without a provable native identity is opaque during sandbox seeding. See [Custom agents](configuration.md#custom-agents). AoE stages a per-session child of that root and mounts it at the agent's canonical container path. Do not add an `extra_volumes` entry for that path, which would shadow the managed mount (AoE warns when one does), and leave the config-dir variables AoE sets in place inside the container.

## Per-session agent stores

Each sandboxed session gets its own agent store on the host, under
`sandbox-v2/<instance-id>` inside the agent's config directory (for example
`~/.claude/sandbox-v2/<id>`). AoE builds that store from the configuration it
declares for the agent: its config files, credentials and authored resources.
The host's native history is never imported, so a session's transcripts,
caches and logs start empty and belong to it alone. A host file AoE does not
declare for that agent, such as one you wrote yourself, stays on the host
instead of being copied. The container mounts the
store at the agent's usual config path, so credentials, hooks and conversation
history belong to one session and `aoe` can resume the right conversation.
Publication is certified only after that declared projection has been staged,
synced and atomically installed. The resulting physical-root certificate binds
the store to its instance; an unproven store is never treated as disposable.

A session keeps the store it was given for as long as AoE can still prove it
wrote that store. When it cannot, each of its content roots is moved intact
under `.aoe-sandbox-recovery/<transaction>/<index>/original` beside the agent's
config directory and a fresh store is seeded in its place, with the session's
original left alone. AoE carries Claude `projects/`, Codex `sessions/` and
`archived_sessions/`, and OpenCode `opencode.db` with its WAL and SHM
from a retired store. It also carries only a uniquely attributable Gemini
`tmp/<project>/chats` session file, Kimi session directory plus its live
index entry, or Prime session file with a matching root header in the managed
`sessions/` directory. Carried conversations keep their native IDs. Claude,
Codex and OpenCode can also continue a structured session backed by that store;
Gemini, Kimi and Prime start a fresh ACP session because carrying the native
conversation does not prove that `session/load` can recover the ACP ID. Missing,
deleted or ambiguous matches remain in recovery and have their IDs reset. Pi,
OMP, Hermes and other unverified conversation histories remain in recovery
instead of being copied.
Host native history is never imported. The next start names retained originals;
isolated history is not replayed automatically.

Sessions created under the older shared-store layout move to a private store the next time they start. Once every session that used the shared store has moved, AoE preserves that store intact under `.aoe-sandbox-recovery/v027-<transaction>/original` instead of deleting it. A session that already had a private store receives only the shared store's top-level configuration and credentials, not its directories of caches, logs, plugins, or unrelated conversation history. If a live sandbox can see the recovery directory, preservation is deferred until that mount is gone.

The first start can therefore be slower; the TUI shows progress. `aoe migrate`
moves every eligible session at once. Trashed and archived sessions keep the
shared store until they are started again.

A sandbox still running during an upgrade remains pending: transcript capture
pauses until it is stopped, isolated and launched again. If native configuration
changes throughout isolation, that session remains pending; `aoe migrate`
continues with other sessions and retries later. A running sandbox whose mounts
AoE cannot prove clear of `.aoe-sandbox-recovery` holds back moving any original
there, and `aoe migrate` names it; launches that retire no original proceed.
`AOE_DEFER_SANDBOX_MIGRATION=1` skips the move for that launch. A running sandbox
carries on on the shared store; a stopped one cannot start until its store has
moved, so drop the variable or run `aoe migrate` before launching it.

### Shared credentials

Claude Code rotates refresh tokens and invalidates the old token, so per-session credential copies would log each other out. All Claude sandboxes mount `sandbox-v2/.credentials.json`; login or refresh in one reaches all. Conversation stores remain private.

When the shared file holds no credential, the next start or refresh seeds it from the freshest macOS Keychain entry, host `.credentials.json`, or earlier session-store copy. Once populated, the host never replaces it automatically; the sandboxes have their own refresh chain. Fresher sandbox credentials from an earlier layout are folded in at that session's next start.

Claude Code empties both tokens after failed authentication, making the shared file eligible for seeding again. Simultaneous refreshes can still conflict: the losing container may empty the shared file and lose the winner's new token.

To seed from a new host login, stop every Claude sandbox, delete only `sandbox-v2/.credentials.json`, and start one. Older containers are recreated when stopped; a running one must be stopped before relaunch. `/logout` in any sandbox revokes all sandboxes' tokens but cannot remove the mounted file; log in again to replace the revoked credential.

### Reclaiming stores

Permanently deleting a sandboxed session removes its store along with its
container only after AoE can prove it owns the native content. An unproven
original is preserved rather than deleted, so a failed delete cannot destroy
content nothing could restore. Reclaim identifies orphan stores by checking live
sessions and retained purge owners across every profile and existing app
namespace; stores stranded before certification, by a delete that kept its
container, or by a delete that failed part-way are found by their instance id
resolving in no profile:

```bash
aoe sandbox reclaim            # report what would go, and how much it frees
aoe sandbox reclaim --delete   # remove it
```

The report is the default because a store holds a copy of the agent's
credentials. A store is removed only when no container for it exists under any
installed runtime: a stopped container can be started again, and nothing a
reclaim can hold would stop it. A store is also kept when a runtime cannot be
asked, and when it was written to in the last fifteen minutes, since a store is
seeded before the session that owns it is recorded. The pass refuses to run
while a store move is in flight or an existing namespace has a missing,
unreadable or unsupported purge-ownership journal. Failed irreversible purges
retain their stores even when the session row no longer exists.

## Worktrees

Git worktrees need the bare repo pattern so the container can reach the repo's git directory. See [Worktrees](worktrees.md#bare-repos).

## Troubleshooting

### Container killed (OOM)

A sandboxed session exits, the container disappears, and `docker inspect <container>` reports `OOMKilled: true`. Docker Desktop runs containers in a Linux VM with a configurable memory ceiling. The VM's kernel can kill containers when memory runs out.

Check Docker Desktop's configured VM memory under **Settings > Resources > Advanced** before choosing `[sandbox] memory_limit`. Raise the VM allocation if needed, and give each container an explicit limit no larger than the VM's available memory. `docker stats --no-stream` shows the limit a running container got. With Docker Engine running directly on Linux, there is no Desktop VM ceiling; a container limit can stop one container from taking all the host's RAM.

### Sandboxed OpenCode fails to start after a container-image upgrade

**Symptoms:** After bumping the sandbox container image, launching or resuming a sandboxed OpenCode session fails on boot with a drizzle or SQLite migration error (e.g. `no such column`, `CREATE TABLE ... already exists`, `Failed to run the query`). Non-sandboxed OpenCode sessions on the same host are unaffected.

**Cause:** On the current private-store layout, each session's OpenCode database normally lives at `~/.local/share/opencode/sandbox-v2/<full-AoE-instance-id>/opencode.db`, with its `-wal` and `-shm` companions. Use the full AoE instance ID, not OpenCode's native `ses_*` ID. The database survives restarts of that session, but is not shared with other sessions. An upgraded sandbox image can ship OpenCode migrations incompatible with that existing database.

**Fix:** Stop the affected session and its container before touching the database. Confirm the host directory actually bind-mounted to OpenCode's data path; `agent_config_dir` overrides or a custom `OPENCODE_DB` path may differ from the default above. Back up `opencode.db`, `opencode.db-wal` and `opencode.db-shm` together, including any companions present, outside the active store. Only then move or remove those three files from the confirmed private store and start a fresh conversation.

This abandons that session's native OpenCode history, not other sessions' conversations or the host's native state. Do not assume the old `ses_*` conversation can resume after its database is removed. To preserve the history, return to the previous image or use an OpenCode migration compatible with the database instead.

Do not delete a shared legacy `sandbox/` database or any `.aoe-sandbox-recovery` original as part of this procedure. Other sessions or deferred migrations may still depend on them. See [Per-session agent stores](#per-session-agent-stores).

**Note:** This is a rare event tied to breaking schema changes in OpenCode releases; most upgrades migrate cleanly. If you hit it repeatedly on the same image bump, please file an issue upstream at [anomalyco/opencode](https://github.com/anomalyco/opencode) with the migration error output.

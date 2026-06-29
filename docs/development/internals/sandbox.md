# Sandbox Internals

This documents sandbox implementation details for contributors. If you just want to use sandboxing, see [Docker Sandbox](../../guides/sandbox.md).

## Lifecycle

1. **Session creation:** `aoe add --sandbox` records the sandbox configuration on the session.
2. **Container start:** Starting the session creates/starts the Docker container with the appropriate volume mounts.
3. **tmux + docker exec:** Host tmux runs `docker exec -it <container> <tool>` to launch the selected agent.
4. **Cleanup:** Removing the session deletes the container (when `auto_cleanup`).

## Container Naming

`aoe-sandbox-{first 8 chars of session_id}`, e.g. `aoe-sandbox-a1b2c3d4`.

## Always-Passed Terminal Env Vars

`TERM`, `COLORTERM`, `FORCE_COLOR`, and `NO_COLOR` are always forwarded into the container for correct UI/theming, regardless of the `environment` config. Everything else comes from the user's `environment` list.

## Shared Agent Config Directories

We share host agent credentials into containers so agents authenticate without re-login. The constraint: bind-mounting the actual host config dirs would let container writes mutate host files. Instead, per agent, we maintain a **shared sandbox directory** that all containers using that agent mount read-write.

For each agent whose host config dir exists, AoE syncs credential files into that agent's sandbox dir, then mounts it RW into every container for that agent. Containers read credentials and write runtime state freely without touching host config. In-container changes (permission approvals, settings tweaks) persist across sessions because all containers share one directory. If an agent's host config dir doesn't exist (agent not installed locally), AoE still creates and mounts the sandbox dir so the agent can write auth/state that persists.

Sandbox dirs are **never auto-deleted**, not even when all sandboxed sessions are removed. This is deliberate: a later sandbox reuses the accumulated state instead of re-prompting setup.

### What Gets Synced

- **Top-level files** from each agent's config dir (auth tokens, credentials, config). Subdirectories are skipped by default to keep the sandbox dir small.
- **Specific subdirectories** listed per agent (e.g. Claude Code's `plugins/` and `skills/`, copied recursively so extensions work in-container).
- **Seed files (write-once)** where needed (e.g. Claude Code gets a minimal `hasCompletedOnboarding` flag to skip the first-run wizard). Seeds are only written if absent, so in-container changes survive.

### Platform-Specific Authentication

- **Linux:** Credential files (e.g. `.credentials.json`) live in the agent's config dir and sync automatically.
- **macOS:** Some agents store credentials in the Keychain, not on disk. AoE extracts them at sync time and writes them as files in the sandbox dir so the container can authenticate. Claude Code OAuth tokens are extracted from the Keychain and written as `.credentials.json`. If there's no Keychain entry (e.g. you auth via `ANTHROPIC_API_KEY`), the sandbox dir still works; pass the key via the `environment` config.

### Credential Refresh

Host credentials are re-synced on **every session start**, not just first creation. Re-authenticating or updating credentials on the host is picked up on the next start. Container-specific state (permission approvals, runtime config) is not overwritten during refresh.

### Sandbox Directory Location

Each agent's shared sandbox dir lives inside that agent's own config dir as a `sandbox/` subdirectory, e.g. `~/.claude/sandbox/`. All containers share it. Deleting the agent's config dir removes everything for that agent including the sandbox dir; to reset just sandbox state, delete the `sandbox/` subdir (re-created on next start).

### Named-Volume Migration

Older AoE stored agent auth in named Docker volumes (e.g. `aoe-claude-auth`). On upgrade, AoE migrates that data into the sandbox dirs automatically. The old volumes are **not** deleted; remove them manually once confirmed:

```bash
docker volume rm aoe-claude-auth aoe-opencode-auth aoe-codex-auth aoe-gemini-auth aoe-vibe-auth
```

## Structured View Inside the Sandbox

Agent-view sessions can run inside the container. When both are enabled, the structured view runner wraps the ACP agent in `docker exec`, so the adapter binary must exist inside the container. The published `aoe-sandbox` image bundles the npm-distributed ACP adapters:

- `claude-agent-acp` (`@agentclientprotocol/claude-agent-acp`, pinned to the host floor; see `docker/Dockerfile`)
- `codex-acp` (`@agentclientprotocol/codex-acp`)
- `pi-acp`

Native adapters that share a binary with the underlying CLI (`opencode acp`, `gemini --acp`, `vibe-acp`) work because the CLI is already installed. A **custom sandbox image** must install the same adapters, or the handshake fails with `agent did not complete the ACP initialize handshake within 30s` (the agent process exits with status 127 the moment the runner exec's it).

## Claude on Vertex AI

If `CLAUDE_CODE_USE_VERTEX` is set and non-empty on the host, AoE wires up Claude+Vertex sessions automatically (only when the active agent is `claude`; other agents get neither the vars nor the cred mount even if the host flag is set):

- `CLAUDE_CODE_USE_VERTEX`, `ANTHROPIC_VERTEX_PROJECT_ID`, `ANTHROPIC_VERTEX_REGION`, and `CLOUD_ML_REGION` are forwarded when set.
- GCP Application Default Credentials are bind-mounted read-only at the well-known container path `/root/.config/gcloud/application_default_credentials.json`. AoE uses `$GOOGLE_APPLICATION_CREDENTIALS` if set, else `~/.config/gcloud/application_default_credentials.json`. `GOOGLE_APPLICATION_CREDENTIALS` itself is not forwarded; client libraries discover the well-known path.

`ANTHROPIC_API_KEY` is not auto-forwarded; list it in `sandbox.environment` if you want it in-container.

## GitHub Authentication with `GH_TOKEN`

Forwarding `GH_TOKEN` (e.g. `"GH_TOKEN=$GH_TOKEN"` in `sandbox.environment`) lets both `gh` and plain `git push` authenticate against `github.com` inside the container. AoE seeds a scoped credential helper in the sandbox gitconfig that reads the token at push time; no credential is written to disk.

Security notes:

- The helper only fires for `https://github.com` remotes; other hosts are unaffected.
- Any process in the sandbox can obtain the token via `git credential fill`. Prefer **fine-grained** PATs limited to the repositories the agent should push to.
- If `GH_TOKEN` is unset at push time the helper stays silent and git falls through to its normal credential flow. Unset the env var to temporarily disable sandboxed pushes without deleting the gitconfig.

# Features

Everything Agent of Empires ships, grouped by what it does, each linking to its full guide. New here? Read the [Quick Start](quick-start.md) first; this page is the inventory.

## Surfaces

### TUI dashboard

A keyboard-driven terminal interface to create, monitor, and manage sessions. Status column at a glance, paired shell view with `t`, diff view with `d`, settings with `s`. Press `?` for help; the bottom bar shows context keybindings.

See the [Quick Start](quick-start.md) for the keyboard tour.

### Web dashboard (Beta)

Browser access to the same sessions: real terminal in the page, switch sessions, type into the terminal, review diffs. Installable as a PWA on desktop ("Install Agent of Empires" in Chrome) and on iOS ("Add to Home Screen"). Token-based auth by default; QR + passphrase pairing when you expose it remotely.

[Web Dashboard guide](guides/web-dashboard.md)

### Structured View

The web dashboard's default rendering: mobile-first native rendering of agent state via the Agent Client Protocol (ACP), with plan panels, tool-call cards, and swipe-to-approve flows instead of raw terminal bytes. Every ACP-capable agent uses it by default; flip a session to terminal view for raw tmux rendering.

[Structured view guide](structured-view.md), [per-agent feature matrix](structured-view.md#feature-matrix)

### CLI

Create, monitor, and control agents from the command line. Integrates with workflow tools like OpenClaw. Scriptable for batch operations and CI.

[CLI Reference](cli/reference.md)

### HTTP API

REST endpoints for driving sessions from external orchestrators. Same operations as the CLI; useful when another service or agent needs to spawn and monitor AoE sessions.

[HTTP API Reference](api.md)

### Remote phone access

Press `R` in the TUI to expose the web dashboard over HTTPS with QR + passphrase auth. Uses Tailscale Funnel when available for a stable URL that survives restarts, falling back to Cloudflare Tunnel. Installs as a PWA on your phone, so notifications keep working after you put the device down.

[Remote Phone Access guide](guides/remote-phone-access.md)

## Agents

### Multi-agent support

AoE drives Claude Code, OpenCode, Mistral Vibe, Codex CLI, Gemini CLI, Cursor CLI, Copilot CLI, Pi.dev, Factory Droid, Hermes, Kiro CLI, and Qwen Code, auto-detecting which are installed and listing them in the new-session picker.

For per-agent structured-view support (which agents render plan panels, which tools are recognized), see the [Structured view feature matrix](structured-view.md#feature-matrix).

### Agent command overrides

Wrap any agent in a custom script or sandboxed launcher. Useful for injecting environment variables, swapping in a containerized runtime, or pinning a specific binary path per profile or repo.

[Agent Command Overrides guide](guides/agent-override.md)

## Repo and workspace

### Git worktrees

Create a session and AoE creates a branch + worktree automatically. Delete the session and AoE cleans up. Run parallel agents on different branches of the same repo without touching your main checkout.

[Git Worktrees guide](guides/worktrees.md)

### Multi-repo workspaces

Drive a single session across several git repositories. The project registry and multi-select pickers let one agent reach into more than one repo at once, for tasks that span services or sibling monorepos.

[Multi-Repo Workspaces guide](guides/multi-repo-workspaces.md)

### Profiles

Separate workspaces for different projects or clients. Each profile has its own sessions, settings, and configuration overrides.

[Configuration: profiles section](guides/configuration.md#profiles)

### Repo config and hooks

Drop a `.agent-of-empires/config.toml` in any repo to pin per-project settings (default agent, sandbox runtime, worktree layout) and hooks that run on session creation or launch.

[Repo Config & Hooks guide](guides/repo-config.md)

## Sandboxing

### Docker sandbox

Run agents inside isolated Docker containers with your project mounted and shared auth volumes for credentials. Configurable volume mounts, persistent auth, automatic container lifecycle tied to the session.

[Docker Sandbox guide](guides/sandbox.md)

Alternative runtimes that share the same code paths:

- [Podman](guides/podman.md), daemonless, optionally rootless; common on Linux.
- [Apple Containers](guides/apple-containers.md), native macOS sandbox on Apple silicon running macOS 26 or later.

## Session lifecycle

### Status detection

Each session reports `Running`, `Waiting`, `Idle`, or `Error` based on tmux pane content and agent-specific heuristics. The TUI, web dashboard, and structured view all show the same status column.

### Auto-stop idle sessions

Set `session.auto_stop_idle_secs` and a plain tmux session that sits `Idle` past the threshold is stopped automatically, leaving a restartable `Stopped` row. Off by default; never stops an attached or recently used session; runs from both the TUI and `aoe serve`. Agent workers use the separate `acp.auto_stop_idle_secs` knob.

[Configuration: session section](guides/configuration.md#session)

### Session resume

Persist and resume Claude Code conversations across reboots, upgrades, and runtime rotations. AoE captures the resume token so the next launch picks up where the agent left off.

[Session Resume guide](guides/session-resume.md)

### tmux persistence

Every agent runs in its own tmux session. Close the TUI, disconnect SSH, or crash your terminal; the agents keep running. Reopen `aoe` and everything is where you left it. `Ctrl+b d` detaches and returns to the TUI.

### Tool sessions

Configure persistent dev-tool sessions (lazygit, yazi, tig, etc.) tied to each agent session's working directory. Hotkey, picker, and command-palette access keep your favorite tools one keystroke away.

[Tool Sessions guide](guides/tool-sessions.md)

## Visibility and review

### Diff view

Review git changes and edit files without leaving the TUI. Browse the diff, jump to a hunk, edit in place, commit when ready.

[Diff View guide](guides/diff-view.md)

### tmux status bar

Surface AoE session info inside your existing tmux status bar. Useful when you spend most of your time inside tmux and want session counts and statuses visible without switching to the TUI.

[tmux Status Bar guide](guides/tmux-status-bar.md)

## Notifications

### Sound effects

Audible cues for status transitions (`Waiting`, `Idle`, `Error`) and structured view approval requests. Configurable per session and globally.

[Sound Effects guide](sounds.md)

### Push notifications

Browser push when an agent is waiting for input, finishes a long-running job, errors out, or requests a structured-view approval. Suppression skips OS banners while you are looking at the TUI or dashboard, so your phone only buzzes when you stepped away.

[Push Notifications guide](push-notifications.md)

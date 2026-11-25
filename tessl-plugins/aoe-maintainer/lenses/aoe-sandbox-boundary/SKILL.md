---
name: aoe-sandbox-boundary
description: Review an Agent of Empires change for ways a sandboxed agent or plugin worker can influence the host, such as host code that reads, follows or mounts paths the sandbox can write, credentials forwarded across the boundary, and worker replies accepted without binding to the authorized worker, and for owner-only host files that can end up readable by others. Use as one lens in a code review run.
---

# Review lens: AoE sandbox boundary

An agent in an AoE sandbox controls everything writable from inside it: the workspace mount, `.git` files and directories, agent config directories, MCP definitions, and any symlink it plants. A plugin worker controls what it sends over its RPC channel. The Daemon and host git run with the user's authority. Review what that control reaches on the host.

## Scope

- **Sandbox-writable paths** Whether host code that reads, resolves, executes from or mounts a path checks it after resolution, rather than trusting a gitdir pointer, hooks directory, config file or symlink the sandbox can rewrite, including nested repositories, bare and separate-git-dir layouts, and paths produced by glob expansion.
- **Host file creation** Whether files holding tokens, passphrases or event logs are created owner-only (mode 0o600) through the existing private-write helpers, including SQLite sidecars and symlink targets, and whether a symlink, missing target or directory at the path makes the code fail closed instead of writing a default-mode file or following the link.
- **Credentials across the boundary** Whether the Daemon token, API keys or user credentials can reach a sandbox or plugin worker through URLs (including query and encoded forms), headers, environment or MCP config, and whether forwarding filters compare canonical forms and fail closed when a parse or decode budget runs out.
- **Worker authority** Whether a reply, hook result or event from a plugin or agent worker is bound to the worker and manifest version that was authorized, rather than accepted by name.

## Method

Start from the paths, URLs and messages the change handles, and ask who can write each one. Treat anything under a sandbox mount, any file the agent can edit, and any worker message as attacker-controlled. Follow it to the host operation it reaches: a git command, a mount, a file open, a permission change, a forwarded request. For each host file operation under a sandbox-writable or agent config directory, check every component of the path, not only the last, for a symlink the sandbox can plant; a guard that refuses a link at the final path does not protect a linked parent directory. Check what happens when a link exists but its target does not.

## Threshold

Report when something a sandboxed agent or plugin worker controls can make host code read, write, execute or disclose something outside what the user granted that sandbox or worker, or when a host file meant to be owner-only can be created or left readable by other local users.

Do not report:
- Dashboard and remote-client authorization, read-only mode, injection, and secrets in logs away from the sandbox and worker boundary; `review-security-and-privacy` owns them.
- Escapes that need the container runtime itself to be compromised.
- Grants the user explicitly opted into, such as skipping permissions, unless the change makes them the default.

## Reporting

- Name the sandbox- or worker-controlled input, the host operation it reaches, and what it gains.
- State the fix: resolve and check after following links, refuse links or create exclusively, mount read-only, compare canonical forms, fail closed on budget, or bind the reply to the authorized worker.

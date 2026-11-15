# Security policy

## Reporting a vulnerability

Report it privately through
[GitHub private vulnerability reporting](https://github.com/agent-of-empires/agent-of-empires/security/advisories/new).
Do not open a public issue, pull request or Discord post about a suspected
vulnerability.

Include the aoe version and OS, the affected surface (TUI, CLI, web dashboard,
daemon, sandbox or plugin), steps to reproduce, and the impact you expect.

## What to expect

- A maintainer acknowledges the report within 3 days.
- Confirmed issues are fixed in the next release; severe ones get an emergency
  release instead of waiting for the weekly one.
- The fix ships with a GitHub security advisory that credits you, unless you
  ask not to be named.

## Supported versions

Only the latest release gets security fixes. Releases ship weekly, so upgrade
instead of waiting for a backport.

## Scope

In scope: aoe failing to enforce a boundary it promises, for example

- web dashboard authentication, tokens and remote access (`aoe serve`);
- [sandbox](../docs/guides/sandbox.md) container isolation;
- secrets reaching other processes, logs or disk;
- [plugin](../docs/development/internals/plugin-system.md) capability checks;
- stored session data.

Out of scope:

- an agent doing what it was asked, with the permissions you gave it;
- vulnerabilities in the agents themselves, such as Claude Code or Codex,
  which belong with their maintainers;
- access that a documented opt-in grants, such as
  `inherit_host_environment`.

# openzoo (Claude Code, paid per call)

[openzoo](https://github.com/staccDOTsol/openzoo) is a local
[x402](https://www.x402.org/) pay-per-call proxy for Claude Code. `openzoo
claude` launches the real Claude Code CLI with `ANTHROPIC_BASE_URL` pointed at
the proxy (`http://localhost:8402/v1`) and forwards every argument untouched to
`claude`. No API key, no Anthropic account, no signup wall: every turn is paid
on chain from a local burner wallet in `~/.openzoo`.

AoE lists it as its own agent, `openzoo`, because the launch and the billing
differ from `claude`; everything else is Claude Code.

## Install

```sh
npm install -g openzoo
aoe agents   # openzoo shows as installed
```

Fund the burner wallet as described in the openzoo README. `openzoo` starts
the proxy itself when nothing is listening on its port, so there is no
separate daemon to keep alive.

## What AoE does with it

Because the pane is the real Claude Code CLI, AoE reuses its Claude support
wholesale:

- **Launch**: `openzoo claude`, with `--dangerously-skip-permissions` (YOLO),
  `--append-system-prompt`, and every other flag placed after the `claude`
  subcommand so Claude Code, not the proxy CLI, parses them. A custom command
  override is used verbatim.
- **Status**: the Claude pane detector plus the same
  `~/.claude/settings.json` hooks (`CLAUDE_CONFIG_DIR` honoured), so running /
  waiting / idle / error land the instant Claude Code reports them.
- **Resume and fork**: `--session-id` / `--resume` / `--fork-session` exactly
  as for `claude`, with the session ID captured from the same hook sidecar and
  the same `~/.claude/projects` transcripts. See
  [Session Resume](./session-resume.md) and [Forking Sessions](./session-fork.md).
- **Quick permission response**: the sidebar `a` / `A` / deny keys send the
  same `1` / `2` / `3` Claude Code expects.
- **Smart rename**: one-shot `openzoo claude -p` titles, paid like any other
  turn.

## Limitations

- **Host only.** The wallet lives in `~/.openzoo` on your machine and the
  sandbox image does not ship openzoo, so the new-session dialog disables
  Docker sandboxing (and, as for every host-only agent, the worktree field)
  for it. Run `claude` in a sandbox if you need isolation.
- **No structured view.** openzoo has no ACP adapter of its own; it runs in
  the terminal view. Structured-view features (including the web "Import from
  Claude" flow) stay on the built-in `claude` agent.
- **Same settings file as `claude`.** Installing or removing AoE's hooks for
  one of the two updates the shared `~/.claude/settings.json`; the hook
  commands are identical, so nothing conflicts.

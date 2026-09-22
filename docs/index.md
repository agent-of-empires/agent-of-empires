# Agent of Empires

[![YouTube](https://img.shields.io/badge/YouTube-channel-red?logo=youtube)](https://www.youtube.com/@agent-of-empires)

A session manager for AI coding agents on Linux and macOS, built on tmux and written in Rust.

AoE runs multiple AI agents in parallel, each in its own tmux session, optionally on its own git branch, optionally inside a Docker container. Use the **TUI dashboard** in your terminal, or the **web dashboard** from any browser on any device.

## See it in action

<iframe width="100%" style="aspect-ratio:16/9;border-radius:8px" src="https://www.youtube-nocookie.com/embed/videoseries?list=UUjGgsnOCZXvvk6UwUQAuwPg" title="Agent of Empires YouTube Channel" frameborder="0" allow="accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture" allowfullscreen
></iframe>

![Agent of Empires Demo](assets/demo.gif)

## Why AoE?

Running several agents in parallel across tasks or branches means juggling terminal windows, git branches, and container lifecycles by hand. AoE handles that:

- **Two front-ends, same sessions.** A TUI in your terminal, and a web dashboard in any browser, phone included.
- **One dashboard for every agent.** Status (running, waiting, idle, error) at a glance, with `t` toggling to the paired shell.
- **Git worktrees built in.** A session creates its branch and worktree, and deleting it cleans them up.
- **Container sandboxing.** Agents run isolated, with your project mounted and auth shared across containers.
- **Per-repo configuration.** A `.agent-of-empires/config.toml` carries project settings and lifecycle hooks.
- **Sessions survive everything.** AoE wraps tmux, so agents keep running when you close the TUI, drop SSH, or crash your terminal.

## Supported agents

Claude Code, OpenCode, Mistral Vibe, Codex CLI, Gemini CLI, Antigravity CLI, Cursor CLI, Copilot CLI, Pi, Oh My Pi (OMP), Factory Droid, Hermes, Kiro CLI, Qwen Code, Kimi Code, and Prime Agent. AoE auto-detects which are installed.

Each agent carries a lifecycle state in AoE's registry, so when a vendor deprecates a CLI, AoE keeps supporting it but marks it everywhere it appears (`aoe agents`, `aoe acp doctor`, session creation, the restart and switch-agent pickers, the web wizard) before you launch one.

<div class="cta-box"> <p><strong>Ready to get started?</strong></p> <p><a href="installation.html">Install AoE</a></p> </div>

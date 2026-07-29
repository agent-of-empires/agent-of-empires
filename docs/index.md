# Agent of Empires

[![YouTube](https://img.shields.io/badge/YouTube-channel-red?logo=youtube)](https://www.youtube.com/@agent-of-empires)

A session manager for AI coding agents on Linux and macOS, built on tmux and written in Rust.

AoE runs multiple AI agents in parallel, each in its own tmux session, optionally on its own git branch, optionally inside a Docker container. Use the **TUI dashboard** in your terminal, or the **web dashboard** from any browser on any device.

## See it in action

<iframe
width="100%"
style="aspect-ratio:16/9;border-radius:8px"
src="https://www.youtube-nocookie.com/embed/videoseries?list=UUjGgsnOCZXvvk6UwUQAuwPg"
title="Agent of Empires YouTube Channel"
frameborder="0"
allow="accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture"
allowfullscreen
></iframe>

![Agent of Empires Demo](assets/demo.gif)

## Why AoE?

Running several AI coding agents in parallel across tasks or branches means juggling terminal windows, git branches, and container lifecycles by hand. AoE handles it for you:

- **Two ways to use it.** A TUI for the terminal, or a web dashboard you can open in any browser, even on your phone. Same sessions, same data.
- **One dashboard for all agents.** See status (running, waiting, idle, error) at a glance. Toggle to paired shell terminals with `t`.
- **Git worktrees built in.** Create a session and AoE creates a branch + worktree automatically. Delete the session and AoE cleans up.
- **Docker sandboxing.** Run agents in isolated containers with your project mounted and auth credentials shared across containers.
- **Per-repo configuration.** Drop a `.agent-of-empires/config.toml` in your repo for project-specific settings and hooks that run on session creation or launch.
- **Sessions survive everything.** AoE wraps tmux, so agents keep running when you close the TUI, disconnect SSH, or your terminal crashes.

## Supported Agents

Claude Code, OpenCode, Mistral Vibe, Codex CLI, Gemini CLI, Antigravity CLI, Cursor CLI, Copilot CLI, Pi, Oh My Pi (OMP), Factory Droid, Hermes, Kiro CLI, Qwen Code, and Kimi Code. AoE auto-detects which are installed.

<div class="cta-box">
<p><strong>Ready to get started?</strong></p>
<p><a href="installation.html">Install AoE</a></p>
</div>

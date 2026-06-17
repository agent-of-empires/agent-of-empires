# aoe-sandbox: Docker image for Agent of Empires sandbox sessions
# This image provides Claude Code, OpenCode, Mistral Vibe, Hermes, Codex CLI, Gemini CLI, Antigravity CLI, Cursor CLI, Copilot CLI, Pi, Kiro CLI, and Qwen Code in an isolated environment

FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    curl \
    git \
    build-essential \
    ca-certificates \
    gnupg \
    jq \
    openssh-client \
    ripgrep \
    fzf \
    unzip \
    && rm -rf /var/lib/apt/lists/*

# Install Node.js (required for Codex CLI)
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# Run as root - IS_SANDBOX=1 allows Claude Code to use --dangerously-skip-permissions
WORKDIR /root

# Install Claude Code CLI (official installer)
RUN curl -fsSL https://claude.ai/install.sh | bash
ENV PATH="/root/.local/bin:${PATH}"

# Install OpenCode
RUN curl -fsSL https://opencode.ai/install | bash
ENV PATH="/root/.opencode/bin:${PATH}"

# Install Hermes (NousResearch/hermes-agent)
# --skip-setup avoids the interactive wizard; the FHS-style root install
# places `hermes` at /usr/local/bin and leaves config under /root/.hermes.
RUN curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh \
    | bash -s -- --skip-setup

# Install Codex CLI (OpenAI)
RUN npm install -g @openai/codex

# Install Gemini CLI (Google)
RUN npm install -g @google/gemini-cli

# Install Antigravity CLI (Google)
RUN curl -fsSL https://antigravity.google/cli/install.sh | bash

# Install Mistral Vibe
RUN curl -LsSf https://mistral.ai/vibe/install.sh | bash
ENV PATH="/root/.vibe/bin:${PATH}"

# Install Cursor CLI
RUN curl -fsSL https://cursor.com/install | bash

# Install Copilot CLI (GitHub)
RUN npm install -g @github/copilot

# Install Pi (pi.dev). The pi-acp adapter (installed below alongside other
# structured view ACP adapters) shells out to the `pi` binary that this package
# provides, so the two packages are paired.
RUN npm install -g @earendil-works/pi-coding-agent

# Install Kiro CLI (AWS)
RUN curl -fsSL https://cli.kiro.dev/install | bash

# Install Qwen Code (Alibaba / QwenLM)
RUN npm install -g @qwen-code/qwen-code

# Install ACP adapters used by `aoe`'s structured view mode when the session is
# sandboxed. The structured view runner `docker exec`s these by their bare binary
# name (see `src/acp/agent_registry.rs`); if they aren't present in
# the image the agent process exits with status 127 and the ACP handshake
# times out. The list mirrors the npm-distributed adapters in
# `src/acp/install_hints.rs`; native adapters (`opencode acp`,
# `gemini --acp`, `vibe-acp`) are already provided by their respective
# CLIs above.
RUN npm install -g \
    @agentclientprotocol/claude-agent-acp@^0.47.0 \
    @zed-industries/codex-acp \
    pi-acp

# Create directories for credential mounts
RUN mkdir -p /root/.claude \
    /root/.config/opencode \
    /root/.local/share/opencode \
    /root/.hermes \
    /root/.vibe \
    /root/.codex \
    /root/.gemini \
    /root/.gemini/antigravity-cli \
    /root/.cursor \
    /root/.copilot \
    /root/.pi \
    /root/.factory \
    /root/.kiro \
    /root/.qwen \
    /root/.ssh

# Allow Claude Code to use --dangerously-skip-permissions as root
ENV IS_SANDBOX=1

# Set working directory
WORKDIR /workspace

# Default command - keep container running for docker exec
CMD ["sleep", "infinity"]

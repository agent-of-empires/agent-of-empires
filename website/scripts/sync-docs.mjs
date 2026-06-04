#!/usr/bin/env node
// Syncs docs/ → website/src/pages/ (guides and docs pages).
//
// Single source of truth: docs/ contains the canonical markdown.
// This script strips the # Title line, rewrites relative links for the
// website URL scheme, and prepends Astro frontmatter.
//
// Generated files are .gitignored; do NOT edit them by hand.

import { readFileSync, writeFileSync, mkdirSync } from "fs";
import { dirname, join } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, "..", "..");
const PAGES_DIR = join(__dirname, "..", "src", "pages");

// All pages to sync. "source" is relative to repo root, "dest" is relative
// to website/src/pages/. Layout path is computed from dest depth.
const PAGES = [
  // --- Guides (docs/guides/ → pages/guides/) ---
  {
    source: "docs/guides/shell-completions.md",
    dest: "guides/shell-completions.md",
    title: "Shell Completions",
    description:
      "Install and refresh tab-completion for the aoe CLI in bash, zsh, fish, PowerShell, and elvish.",
  },
  {
    source: "docs/guides/diff-view.md",
    dest: "guides/diff-view.md",
    title: "Diff View",
    description:
      "Review git changes and edit files directly from the Agent of Empires TUI.",
  },
  {
    source: "docs/guides/repo-config.md",
    dest: "guides/repo-config.md",
    title: "Repository Configuration & Hooks",
    description:
      "Per-repo configuration and hooks for Agent of Empires sessions.",
  },
  {
    source: "docs/guides/mcp-servers.md",
    dest: "guides/mcp-servers.md",
    title: "MCP Servers",
    description:
      "Forward configured MCP servers to structured-view agents via mcp.json.",
  },
  {
    source: "docs/guides/sandbox.md",
    dest: "guides/sandbox.md",
    title: "Docker Sandbox: Quick Reference",
    description:
      "Run AI coding agents in isolated Docker containers with Agent of Empires.",
  },
  {
    source: "docs/guides/tmux-status-bar.md",
    dest: "guides/tmux-status-bar.md",
    title: "tmux Status Bar",
    description:
      "Configure the tmux status bar to display Agent of Empires session information.",
  },
  {
    source: "docs/guides/web-dashboard.md",
    dest: "guides/web-dashboard.md",
    title: "Web Dashboard (Experimental)",
    description:
      "Remote access to AI coding agent sessions from any browser with Agent of Empires.",
  },
  {
    source: "docs/guides/remote-phone-access.md",
    dest: "guides/remote-phone-access.md",
    title: "Remote Access from Your Phone",
    description:
      "Access your Agent of Empires sessions from your phone via Tailscale Funnel or Cloudflare Tunnel with QR pairing.",
  },
  {
    source: "docs/guides/web/dashboard.md",
    dest: "guides/web/dashboard.md",
    title: "Dashboard & Workspaces",
    description:
      "The web dashboard layout: workspace sidebar, status glyphs, the session-creation wizard, command palette, sidebar sort, and triage.",
  },
  {
    source: "docs/guides/web/terminal.md",
    dest: "guides/web/terminal.md",
    title: "Terminal View",
    description:
      "The browser agent and paired terminals: PTY relay, scrollback, reconnect behavior, WebSocket close codes, and read-only mode.",
  },
  {
    source: "docs/guides/web/diff.md",
    dest: "guides/web/diff.md",
    title: "Web Diff View",
    description:
      "Review a session's changes from the browser: the flat / tree changed-files list, per-session base override, and inline review comments.",
  },
  {
    source: "docs/guides/web/settings.md",
    dest: "guides/web/settings.md",
    title: "Settings & Profiles",
    description:
      "The web settings tabs, the profile picker, connected-device tracking, and the step-up elevation gate for persisted config edits.",
  },
  {
    source: "docs/guides/worktrees.md",
    dest: "guides/worktrees.md",
    title: "Worktrees Reference",
    description:
      "Git worktree commands and configuration reference for Agent of Empires.",
  },
  {
    source: "docs/guides/agent-override.md",
    dest: "guides/agent-override.md",
    title: "Agent Command Overrides",
    description:
      "Override agent commands with custom scripts or sandboxed wrappers in Agent of Empires.",
  },
  {
    source: "docs/guides/session-resume.md",
    dest: "guides/session-resume.md",
    title: "Session Resume (Claude)",
    description:
      "Persist and resume Claude Code conversations across reboots, upgrades, and runtime rotations.",
  },
  {
    source: "docs/guides/multi-repo-workspaces.md",
    dest: "guides/multi-repo-workspaces.md",
    title: "Multi-Repo Workspaces",
    description:
      "Drive a single Agent of Empires session across several git repositories with the project registry and multi-select pickers.",
  },
  {
    source: "docs/guides/scratch-sessions.md",
    dest: "guides/scratch-sessions.md",
    title: "Scratch Sessions",
    description:
      "Launch a session in a fresh scratch directory under ~/.agent-of-empires/scratch/ with no project path. The directory is removed when the session is deleted.",
  },
  {
    source: "docs/guides/live-mode.md",
    dest: "guides/live-mode.md",
    title: "Live Mode",
    description:
      "A feels-attached alternative to a full tmux attach: the dashboard stays visible while keystrokes relay to the agent. Covers the Ctrl+B leader menu, the collapsible sidebar, scrolling, and the exit chord.",
  },

  // --- Docs pages (docs/ → pages/docs/) ---
  {
    source: "docs/index.md",
    dest: "docs/index.md",
    title: "Agent of Empires",
    description:
      "Terminal session manager for AI coding agents on Linux and macOS, built on tmux and written in Rust.",
  },
  {
    source: "docs/installation.md",
    dest: "docs/installation.md",
    title: "Installation",
    description:
      "Install Agent of Empires on Linux or macOS via the install script, Homebrew, or from source.",
  },
  {
    source: "docs/quick-start.md",
    dest: "docs/quick-start.md",
    title: "Quick Start",
    description:
      "Get up and running with Agent of Empires in minutes. Create sessions, attach to agents, and use worktrees.",
  },
  {
    source: "docs/development.md",
    dest: "docs/development.md",
    title: "Development",
    description: "Build, run, and test Agent of Empires from source.",
  },
  {
    source: "docs/development/adding-agents.md",
    dest: "docs/development/adding-agents.md",
    title: "Adding a New Agent",
    description:
      "Step-by-step guide for adding support for a new AI coding agent to AoE.",
  },
  {
    source: "docs/development/adding-settings.md",
    dest: "docs/development/adding-settings.md",
    title: "Adding a Setting",
    description:
      "How to add a configuration setting with the single-source schema that drives the TUI, web dashboard, and server.",
  },
  {
    source: "docs/development/logging.md",
    dest: "docs/development/logging.md",
    title: "Logging",
    description:
      "Logging targets, env-var matrix, runtime control endpoint, and browser-side error relay for Agent of Empires.",
  },
  {
    source: "docs/development/playwright.md",
    dest: "docs/development/playwright.md",
    title: "Playwright + Vitest testing",
    description:
      "Long-form reference for the web dashboard test pipeline: mocked vs live Playwright, Vitest contract tests, fake ACP agent, coverage matrix, coverage reports.",
  },
  {
    source: "docs/development/releases.md",
    dest: "docs/development/releases.md",
    title: "Releases",
    description:
      "Weekly release cadence, automated staging PR, post-merge tagger, and emergency-release path for Agent of Empires maintainers.",
  },
  {
    source: "docs/development/web-dashboard.md",
    dest: "docs/development/web-dashboard.md",
    title: "Web Dashboard Development",
    description:
      "Build the web dashboard from source, run the frontend dev workflow (cargo xtask dev and manual Vite + VITE_PROXY), and the server architecture.",
  },
  {
    source: "docs/sounds.md",
    dest: "docs/sounds.md",
    title: "Sound Effects",
    description:
      "Configure audio feedback for agent state transitions in Agent of Empires.",
  },
  {
    source: "docs/push-notifications.md",
    dest: "docs/push-notifications.md",
    title: "Push Notifications",
    description:
      "Browser and PWA push notifications for Agent of Empires session status changes and structured view approvals.",
  },
  {
    source: "docs/features.md",
    dest: "docs/features.md",
    title: "Features",
    description:
      "Canonical inventory of every Agent of Empires feature, grouped by surface and capability, with links to each guide.",
  },
  {
    source: "docs/github-integration.md",
    dest: "docs/github-integration.md",
    title: "GitHub Integration",
    description:
      "How Agent of Empires resolves a GitHub token, the per-failure hints it shows, and what is deferred to follow-ups.",
  },
  {
    source: "docs/guides/podman.md",
    dest: "guides/podman.md",
    title: "Podman",
    description:
      "Run Agent of Empires sandboxes on Podman, a daemonless and rootless Docker alternative.",
  },
  {
    source: "docs/guides/apple-containers.md",
    dest: "guides/apple-containers.md",
    title: "Apple Containers",
    description:
      "Run Agent of Empires sandboxes on Apple's native macOS container runtime on Apple silicon.",
  },
  {
    source: "docs/guides/configuration.md",
    dest: "docs/guides/configuration.md",
    title: "Configuration Reference",
    description:
      "Complete configuration reference for Agent of Empires settings, profiles, and repo config.",
  },
  {
    source: "docs/cli/reference.md",
    dest: "docs/cli/reference.md",
    title: "CLI Reference",
    description:
      "Complete command-line reference for the aoe CLI tool.",
  },
  {
    source: "docs/structured-view.md",
    dest: "docs/structured-view.md",
    title: "Structured View (Web Dashboard)",
    description:
      "The web dashboard's default structured view: native rendering of AI agent state via the Agent Client Protocol (ACP). Plan panels, tool-call cards, swipe-to-approve, multi-provider support.",
  },
  {
    source: "docs/structured-view/multi-agent.md",
    dest: "docs/structured-view/multi-agent.md",
    title: "Structured View Multi-Agent Support",
    description:
      "Per-agent feature matrix: claude, codex, opencode, gemini. Covers profile data, supported tools, and known limitations.",
  },
  {
    source: "docs/structured-view/setup.md",
    dest: "docs/structured-view/setup.md",
    title: "Structured View Setup",
    description:
      "Confirm prerequisites with aoe acp doctor, pick structured view or terminal view per session, attach across machines, and drive it from the CLI.",
  },
  {
    source: "docs/structured-view/interface.md",
    dest: "docs/structured-view/interface.md",
    title: "Structured View Interface",
    description:
      "The TUI and web structured views: keybinds, composer behavior on desktop and touch, queued prompts, and timeline card grouping.",
  },
  {
    source: "docs/structured-view/controls.md",
    dest: "docs/structured-view/controls.md",
    title: "Structured View Modes, Approvals & Model Controls",
    description:
      "Permission modes, YOLO and bypassPermissions, approval cards and notifications, plus the model and reasoning-effort selectors.",
  },
  {
    source: "docs/structured-view/persistence.md",
    dest: "docs/structured-view/persistence.md",
    title: "Structured View Persistence & Recovery",
    description:
      "How agent workers survive an aoe serve restart, what session deletion cleans up, and how conversation context is rehydrated.",
  },
  {
    source: "docs/structured-view/troubleshooting.md",
    dest: "docs/structured-view/troubleshooting.md",
    title: "Structured View Troubleshooting",
    description:
      "The structured view security model plus a field guide to every failure mode: doctor errors, spawn failures, rate limits, stuck turns, and the watchdog.",
  },
  {
    source: "docs/guides/tool-sessions.md",
    dest: "guides/tool-sessions.md",
    title: "Tool Sessions",
    description:
      "Configure persistent dev-tool sessions (lazygit, yazi, tig, etc.) tied to each agent session's working directory, with hotkey, picker, and command-palette access.",
  },
  {
    source: "docs/api.md",
    dest: "docs/api.md",
    title: "HTTP API Reference",
    description:
      "REST endpoints for driving Agent of Empires sessions from external orchestrators.",
  },
  {
    source: "docs/telemetry.md",
    dest: "docs/telemetry.md",
    title: "Telemetry",
    description:
      "How Agent of Empires' anonymous, opt-in usage telemetry works: what is and isn't collected, the DO_NOT_TRACK override, and how to enable or disable it.",
  },
];

// Every known docs path → website URL, used for link rewriting.
const URL_MAP = {
  // Docs pages
  "docs/index.md": "/docs/",
  "docs/installation.md": "/docs/installation/",
  "docs/quick-start.md": "/docs/quick-start/",
  "docs/sounds.md": "/docs/sounds/",
  "docs/push-notifications.md": "/docs/push-notifications/",
  "docs/features.md": "/docs/features/",
  "docs/github-integration.md": "/docs/github-integration/",
  "docs/development.md": "/docs/development/",
  "docs/development/adding-agents.md": "/docs/development/adding-agents/",
  "docs/development/adding-settings.md": "/docs/development/adding-settings/",
  "docs/development/logging.md": "/docs/development/logging/",
  "docs/development/playwright.md": "/docs/development/playwright/",
  "docs/development/releases.md": "/docs/development/releases/",
  "docs/development/web-dashboard.md": "/docs/development/web-dashboard/",
  "docs/guides/configuration.md": "/docs/guides/configuration/",
  "docs/cli/reference.md": "/docs/cli/reference/",
  "docs/structured-view.md": "/docs/structured-view/",
  "docs/structured-view/multi-agent.md": "/docs/structured-view/multi-agent/",
  "docs/structured-view/setup.md": "/docs/structured-view/setup/",
  "docs/structured-view/interface.md": "/docs/structured-view/interface/",
  "docs/structured-view/controls.md": "/docs/structured-view/controls/",
  "docs/structured-view/persistence.md": "/docs/structured-view/persistence/",
  "docs/structured-view/troubleshooting.md": "/docs/structured-view/troubleshooting/",
  "docs/api.md": "/docs/api/",
  "docs/telemetry.md": "/docs/telemetry/",
  // Guides
  "docs/guides/shell-completions.md": "/guides/shell-completions/",
  "docs/guides/diff-view.md": "/guides/diff-view/",
  "docs/guides/repo-config.md": "/guides/repo-config/",
  "docs/guides/mcp-servers.md": "/guides/mcp-servers/",
  "docs/guides/sandbox.md": "/guides/sandbox/",
  "docs/guides/tmux-status-bar.md": "/guides/tmux-status-bar/",
  "docs/guides/web-dashboard.md": "/guides/web-dashboard/",
  "docs/guides/web/dashboard.md": "/guides/web/dashboard/",
  "docs/guides/web/terminal.md": "/guides/web/terminal/",
  "docs/guides/web/diff.md": "/guides/web/diff/",
  "docs/guides/web/settings.md": "/guides/web/settings/",
  "docs/guides/remote-phone-access.md": "/guides/remote-phone-access/",
  "docs/guides/worktrees.md": "/guides/worktrees/",
  "docs/guides/agent-override.md": "/guides/agent-override/",
  "docs/guides/session-resume.md": "/guides/session-resume/",
  "docs/guides/multi-repo-workspaces.md": "/guides/multi-repo-workspaces/",
  "docs/guides/scratch-sessions.md": "/guides/scratch-sessions/",
  "docs/guides/live-mode.md": "/guides/live-mode/",
  "docs/guides/tool-sessions.md": "/guides/tool-sessions/",
  "docs/guides/podman.md": "/guides/podman/",
  "docs/guides/apple-containers.md": "/guides/apple-containers/",
};

const GITHUB_BASE =
  "https://github.com/agent-of-empires/agent-of-empires/blob/main/";

function rewriteLinks(content, sourceDir) {
  // Rewrite markdown links to .md files: [text](target.md) or [text](target.md#anchor)
  content = content.replace(
    /\]\(([^)]+\.md(?:#[^)]*)?)\)/g,
    (_match, link) => {
      if (link.startsWith("http://") || link.startsWith("https://")) {
        return `](${link})`;
      }
      const hashIdx = link.indexOf("#");
      const targetFile = hashIdx >= 0 ? link.slice(0, hashIdx) : link;
      const anchor = hashIdx >= 0 ? link.slice(hashIdx) : "";
      const resolved = join(sourceDir, targetFile)
        .replace(/\\/g, "/")
        .replace(/^\.\//, "");
      const websiteUrl = URL_MAP[resolved];
      if (websiteUrl) {
        return `](${websiteUrl}${anchor})`;
      }
      return `](${GITHUB_BASE}${resolved}${anchor})`;
    }
  );

  // Rewrite HTML href links to .md or .html files (e.g., <a href="installation.html">)
  content = content.replace(
    /href="([^"]+\.(?:md|html)(?:#[^"]*)?)"/g,
    (_match, link) => {
      if (link.startsWith("http://") || link.startsWith("https://")) {
        return `href="${link}"`;
      }
      const hashIdx = link.indexOf("#");
      const targetFile = hashIdx >= 0 ? link.slice(0, hashIdx) : link;
      const anchor = hashIdx >= 0 ? link.slice(hashIdx) : "";
      // Normalize .html to .md for lookup
      const targetMd = targetFile.replace(/\.html$/, ".md");
      const resolved = join(sourceDir, targetMd)
        .replace(/\\/g, "/")
        .replace(/^\.\//, "");
      const websiteUrl = URL_MAP[resolved];
      if (websiteUrl) {
        return `href="${websiteUrl}${anchor}"`;
      }
      return _match;
    }
  );

  // Rewrite relative image/asset paths to absolute (/assets/...).
  // The build copies docs/assets/* to website/public/assets/. Both
  // `assets/foo.png` (used by root-level docs like docs/index.md) and
  // `../assets/foo.png` (used by guides at docs/guides/foo.md) map to
  // the same place, so normalize both to `/assets/`. This catches
  // markdown links, markdown images (![alt](..) contains ](..)), and
  // HTML-less references alike. `(?:\.\.\/)*` handles any depth of
  // parent-directory hops so deeper-nested docs future-proof through.
  content = content.replace(/\]\((?:\.\.\/)*assets\//g, "](/assets/");

  return content;
}

function computeLayoutPath(dest) {
  // Layout is at website/src/layouts/Docs.astro.
  // A page at website/src/pages/guides/foo.md needs ../../layouts/Docs.astro
  // A page at website/src/pages/docs/cli/ref.md needs ../../../layouts/Docs.astro
  const segments = dirname(dest).split("/").filter((s) => s !== ".");
  const depth = segments.length + 1; // +1 to go from pages/ up to src/
  return "../".repeat(depth) + "layouts/Docs.astro";
}

function escapeYaml(str) {
  if (/[:"'\\]/.test(str)) {
    return `"${str.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
  }
  return str;
}

console.log("Syncing docs to website...");

for (const page of PAGES) {
  const sourcePath = join(ROOT, page.source);
  let content = readFileSync(sourcePath, "utf8");

  // Strip the leading # Title line (first non-empty line starting with #)
  content = content.replace(/^# .+\n\n?/, "");

  // Rewrite links
  const sourceDir = dirname(page.source);
  content = rewriteLinks(content, sourceDir);

  // Prepend Astro frontmatter
  const layout = computeLayoutPath(page.dest);
  const frontmatter = [
    "---",
    `layout: ${layout}`,
    `title: ${escapeYaml(page.title)}`,
    `description: ${escapeYaml(page.description)}`,
    "---",
    "",
    "",
  ].join("\n");

  const destPath = join(PAGES_DIR, page.dest);
  mkdirSync(dirname(destPath), { recursive: true });
  writeFileSync(destPath, frontmatter + content);

  console.log(`  ${page.source} -> ${page.dest}`);
}

// Verify every synced page appears in docsNav.ts
const navPath = join(__dirname, "..", "src", "data", "docsNav.ts");
const navSource = readFileSync(navPath, "utf8");
const navHrefs = new Set([...navSource.matchAll(/href:\s*"([^"]+)"/g)].map((m) => m[1]));
let missing = 0;
for (const page of PAGES) {
  const url = "/" + page.dest.replace(/\.md$/, "/").replace(/\/index\/$/, "/");
  if (!navHrefs.has(url)) {
    console.error(`  WARNING: ${url} (from ${page.source}) is not in docsNav.ts`);
    missing++;
  }
}
if (missing > 0) {
  console.error(`\n${missing} page(s) missing from sidebar navigation (website/src/data/docsNav.ts)`);
  process.exit(1);
}

console.log("Done.");

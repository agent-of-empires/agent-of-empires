export interface NavItem {
  title: string;
  href: string;
  /** Short blurb shown on the guides landing page. Unused by the sidebar. */
  description?: string;
}

export interface NavSection {
  title: string;
  items: NavItem[];
}

export const docsNav: NavSection[] = [
  {
    title: "Getting Started",
    items: [
      { title: "Introduction", href: "/docs/" },
      { title: "Features", href: "/docs/features/" },
      { title: "Installation", href: "/docs/installation/" },
      { title: "Quick Start", href: "/docs/quick-start/" },
    ],
  },
  {
    title: "Guides",
    items: [
      { title: "Docker Sandbox", href: "/guides/sandbox/", description: "Run AI coding agents in isolated Docker containers." },
      { title: "Podman", href: "/guides/podman/", description: "Use Podman as a rootless alternative to Docker for sandboxing." },
      { title: "Apple Containers", href: "/guides/apple-containers/", description: "Sandbox agents with Apple's native container framework on macOS." },
      { title: "Live Mode", href: "/guides/live-mode/", description: "Watch a session stream live and type into it from the TUI." },
      { title: "Repo Config & Hooks", href: "/guides/repo-config/", description: "Per-repo configuration and lifecycle hooks for sessions." },
      { title: "Git Worktrees", href: "/guides/worktrees/", description: "How AoE creates and cleans up a git worktree per session." },
      { title: "Multi-Repo Workspaces", href: "/guides/multi-repo-workspaces/", description: "Drive one session across several git repositories at once." },
      { title: "Scratch Sessions", href: "/guides/scratch-sessions/", description: "Throwaway sessions for quick experiments without a worktree." },
      { title: "Diff View", href: "/guides/diff-view/", description: "Review git changes and edit files from the TUI." },
      { title: "tmux Status Bar", href: "/guides/tmux-status-bar/", description: "Show live session status in your tmux status bar." },
      { title: "Agent Command Overrides", href: "/guides/agent-override/", description: "Customize the command used to launch each agent." },
      { title: "Tool Sessions", href: "/guides/tool-sessions/", description: "Run plain shell or tool sessions alongside your agents." },
      { title: "MCP Servers", href: "/guides/mcp-servers/", description: "Forward configured MCP servers to structured-view agents." },
      { title: "Session Resume (Claude)", href: "/guides/session-resume/", description: "Resume a previous Claude Code conversation in a session." },
      { title: "Shell Completions", href: "/guides/shell-completions/", description: "Install and refresh tab-completion for the aoe CLI." },
      { title: "Sound Effects", href: "/docs/sounds/", description: "Play sounds when agents need input or finish work." },
      { title: "Push Notifications", href: "/docs/push-notifications/", description: "Get notified when a session needs your attention." },
    ],
  },
  {
    title: "Web Dashboard",
    items: [
      { title: "Overview", href: "/guides/web-dashboard/", description: "Remote access to your sessions from any browser." },
      { title: "Dashboard & Workspaces", href: "/guides/web/dashboard/", description: "The dashboard layout: workspace sidebar, status glyphs, and the session wizard." },
      { title: "Terminal View", href: "/guides/web/terminal/", description: "A real terminal in the browser, backed by your tmux session." },
      { title: "Diff View", href: "/guides/web/diff/", description: "Review and stage git changes from the web dashboard." },
      { title: "Settings & Profiles", href: "/guides/web/settings/", description: "Manage settings and configuration profiles from the web." },
      { title: "Remote Phone Access", href: "/guides/remote-phone-access/", description: "Expose the dashboard over HTTPS with QR pairing." },
    ],
  },
  {
    title: "Structured View",
    items: [
      { title: "Overview", href: "/docs/structured-view/" },
      { title: "Interface", href: "/docs/structured-view/interface/" },
      { title: "Modes, Approvals & Models", href: "/docs/structured-view/controls/" },
      { title: "Troubleshooting", href: "/docs/structured-view/troubleshooting/" },
    ],
  },
  {
    title: "Reference",
    items: [
      { title: "CLI Reference", href: "/docs/cli/reference/" },
      { title: "HTTP API Reference", href: "/docs/api/" },
      { title: "Telemetry", href: "/docs/telemetry/" },
      { title: "GitHub Integration", href: "/docs/github-integration/" },
      { title: "Configuration", href: "/docs/guides/configuration/" },
    ],
  },
  {
    title: "Contributing",
    items: [
      { title: "Development", href: "/docs/development/" },
      { title: "Adding a New Agent", href: "/docs/development/adding-agents/" },
      { title: "Adding a Setting", href: "/docs/development/adding-settings/" },
      { title: "Logging", href: "/docs/development/logging/" },
      { title: "Playwright + Vitest testing", href: "/docs/development/playwright/" },
      { title: "Releases", href: "/docs/development/releases/" },
      {
        title: "Web Dashboard Development",
        href: "/docs/development/web-dashboard/",
      },
    ],
  },
  {
    title: "Internals (Contributor)",
    items: [
      { title: "Structured View Internals", href: "/docs/development/internals/structured-view/" },
      { title: "Sandbox Internals", href: "/docs/development/internals/sandbox/" },
      { title: "Session & Worktree Internals", href: "/docs/development/internals/sessions/" },
    ],
  },
];

export function getFlatNavItems(): NavItem[] {
  return docsNav.flatMap((section) => section.items);
}

/** Section titles featured on the /guides/ landing page, in display order. */
const GUIDE_SECTION_TITLES = ["Guides", "Web Dashboard"];

/**
 * Sections to feature on the guides landing page. Derived from `docsNav` so the
 * landing page and the docs sidebar can never drift apart: add a guide once,
 * here, and it shows up in both places.
 */
export function getGuideSections(): NavSection[] {
  return GUIDE_SECTION_TITLES.map((title) => {
    const section = docsNav.find((s) => s.title === title);
    if (!section) {
      throw new Error(`getGuideSections: no docsNav section titled "${title}"`);
    }
    return section;
  });
}

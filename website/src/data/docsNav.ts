export interface NavItem {
  title: string;
  href: string;
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
      { title: "Docker Sandbox", href: "/guides/sandbox/" },
      { title: "Podman", href: "/guides/podman/" },
      { title: "Apple Containers", href: "/guides/apple-containers/" },
      { title: "Live Mode", href: "/guides/live-mode/" },
      { title: "Web Dashboard", href: "/guides/web-dashboard/" },
      { title: "Cockpit (Native Agent Rendering)", href: "/docs/cockpit/" },
      { title: "Cockpit Multi-Agent Support", href: "/docs/cockpit/multi-agent/" },
      { title: "Remote Phone Access", href: "/guides/remote-phone-access/" },
      { title: "Repo Config & Hooks", href: "/guides/repo-config/" },
      { title: "Git Worktrees", href: "/guides/worktrees/" },
      { title: "Multi-Repo Workspaces", href: "/guides/multi-repo-workspaces/" },
      { title: "Scratch Sessions", href: "/guides/scratch-sessions/" },
      { title: "Diff View", href: "/guides/diff-view/" },
      { title: "tmux Status Bar", href: "/guides/tmux-status-bar/" },
      { title: "Agent Command Overrides", href: "/guides/agent-override/" },
      { title: "Tool Sessions", href: "/guides/tool-sessions/" },
      { title: "Session Resume (Claude)", href: "/guides/session-resume/" },
      { title: "Sound Effects", href: "/docs/sounds/" },
      { title: "Push Notifications", href: "/docs/push-notifications/" },
    ],
  },
  {
    title: "Reference",
    items: [
      { title: "CLI Reference", href: "/docs/cli/reference/" },
      { title: "HTTP API Reference", href: "/docs/api/" },
      { title: "GitHub Integration", href: "/docs/github-integration/" },
      { title: "Configuration", href: "/docs/guides/configuration/" },
    ],
  },
  {
    title: "Contributing",
    items: [
      { title: "Development", href: "/docs/development/" },
      { title: "Adding a New Agent", href: "/docs/development/adding-agents/" },
      { title: "Logging", href: "/docs/development/logging/" },
      { title: "Playwright + Vitest testing", href: "/docs/development/playwright/" },
      { title: "Releases", href: "/docs/development/releases/" },
    ],
  },
];

export function getFlatNavItems(): NavItem[] {
  return docsNav.flatMap((section) => section.items);
}

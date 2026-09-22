// Engine-independent tour steps. Attach anchors only via `tourAnchor()` so the drift guard in tourSteps.test.ts catches renames.

import type { ShortcutId } from "./shortcuts";

/** Values land in the DOM as `data-tour`; keep them on stable region containers. */
export const TOUR_ANCHORS = {
  topbar: "topbar",
  topbarMore: "topbar-more",
  sidebar: "sidebar",
  sidebarSettings: "sidebar-settings",
  dashboardNewSession: "dashboard-new-session",
  settingsWorktree: "settings-worktree",
  settingsPlugins: "settings-plugins",
  settingsAgentDefaults: "settings-agent-defaults",
  rightPanel: "right-panel",
  composer: "acp-composer",
  modePicker: "acp-mode-picker",
  queueSend: "acp-queue-send",
} as const;

export type TourAnchorId = (typeof TOUR_ANCHORS)[keyof typeof TOUR_ANCHORS];

/** Steps never show outside their scope, so a missing anchor is legitimately absent, not a regression. */
export type TourScope = "dashboard" | "session" | "structured-view";

/** References a registered shortcut by id so the rendered chord cannot drift from the binding. */
export interface TourShortcutHint {
  id: ShortcutId;
  verb: string;
}

export interface TourStep {
  /** Also the react-joyride step id. */
  id: string;
  anchor: TourAnchorId;
  scopes: readonly TourScope[];
  title: string;
  body: string;
  shortcutHints?: readonly TourShortcutHint[];
  writableOnly?: boolean;
  desktopOnly?: boolean;
  /** The anchor mounts only after the runner navigates to `/settings/<tab>`, so it skips the launch-time DOM probe. */
  settingsTab?: "worktree" | "plugins" | "structured-view";
  /** Needed for settings steps whose tab CityHall hides, since they bypass the DOM probe. */
  hiddenInCityhall?: boolean;
  /** For anchors already in view whose surroundings grow after mount, which otherwise loops joyride's scroll-into-view. */
  disableScrolling?: boolean;
}

export function tourSelector(anchor: TourAnchorId): string {
  return `[data-tour="${anchor}"]`;
}

/** The only sanctioned way to attach a tour anchor; spread it onto the element. */
export function tourAnchor(anchor: TourAnchorId): {
  "data-tour": TourAnchorId;
} {
  return { "data-tour": anchor };
}

export const TOUR_STEPS: readonly TourStep[] = [
  {
    id: "topbar",
    anchor: TOUR_ANCHORS.topbar,
    scopes: ["dashboard", "session", "structured-view"],
    title: "Command bar",
    body: "Jump anywhere from the command palette: switch sessions, open settings, toggle panels.",
    shortcutHints: [{ id: "palette", verb: "opens the palette" }],
  },
  {
    id: "sidebar",
    anchor: TOUR_ANCHORS.sidebar,
    scopes: ["dashboard", "session", "structured-view"],
    title: "Workspaces and sessions",
    body: "Your sessions are grouped by workspace here. Pick one to open its terminal or structured view.",
    shortcutHints: [{ id: "sidebar", verb: "toggles the sidebar" }],
  },
  {
    id: "new-session",
    anchor: TOUR_ANCHORS.dashboardNewSession,
    scopes: ["dashboard"],
    title: "Start a session",
    body: "Launch a new agent session: pick a project, choose an agent, and go.",
    shortcutHints: [
      { id: "new", verb: "opens the wizard" },
      { id: "newScratch", verb: "starts a scratch session" },
    ],
    writableOnly: true,
  },
  {
    id: "settings",
    anchor: TOUR_ANCHORS.sidebarSettings,
    scopes: ["dashboard", "session", "structured-view"],
    title: "Settings and profiles",
    body: "Tune sandboxing, worktrees, sounds, devices, and per-profile overrides here.",
    shortcutHints: [{ id: "settings", verb: "opens settings" }],
  },
  {
    id: "settings-worktree",
    anchor: TOUR_ANCHORS.settingsWorktree,
    settingsTab: "worktree",
    scopes: ["dashboard"],
    writableOnly: true,
    desktopOnly: true,
    hiddenInCityhall: true,
    title: "Worktrees keep sessions isolated",
    body: "Worktrees give each session its own branch checkout, so agents never step on each other. They are off by default; enable them here. The path template decides where each checkout lands: {repo-name}, {branch}, and {session-id} expand per session (default ../{repo-name}-worktrees/{branch}). A separate bare-repo template sits under Advanced.",
  },
  {
    id: "settings-plugins",
    anchor: TOUR_ANCHORS.settingsPlugins,
    settingsTab: "plugins",
    scopes: ["dashboard"],
    desktopOnly: true,
    title: "Extend AoE with plugins",
    body: "Browse and manage plugins here. Marketplace searches the aoe-plugin GitHub topic; click Install on a result to add it (you confirm its capabilities first). A featured badge means the maintainers reviewed and pinned that release; unvetted means a matching repo nobody has audited, so install at your own risk.",
  },
  {
    id: "settings-agent-defaults",
    anchor: TOUR_ANCHORS.settingsAgentDefaults,
    settingsTab: "structured-view",
    scopes: ["dashboard"],
    desktopOnly: true,
    hiddenInCityhall: true,
    // The tab grows after mount; see `disableScrolling`.
    disableScrolling: true,
    title: "Set per-agent defaults",
    body: "Pick a default model, mode, and thinking level for each agent here, so new structured-view sessions start the way you want without touching the composer. The choices come from what each agent last advertised, so you set them once per agent and new models appear automatically. Per-model thinking lets one agent think harder on some models than others.",
  },
  {
    id: "right-panel",
    anchor: TOUR_ANCHORS.rightPanel,
    scopes: ["session", "structured-view"],
    title: "Diff and review",
    body: "Review the agent's file changes and send comments back without leaving the session.",
    shortcutHints: [
      { id: "diff", verb: "toggles the diff" },
      { id: "rightPanel", verb: "toggles the panel" },
    ],
    desktopOnly: true,
  },
  {
    id: "composer",
    anchor: TOUR_ANCHORS.composer,
    scopes: ["structured-view"],
    title: "Composer",
    body: "Write instructions to the agent here. Type / for commands and @ to reference files.",
  },
  {
    id: "mode-picker",
    anchor: TOUR_ANCHORS.modePicker,
    scopes: ["structured-view"],
    title: "Agent mode",
    body: "Switch the agent's mode (plan, accept edits, and so on) before you send.",
  },
  {
    id: "queue-send",
    anchor: TOUR_ANCHORS.queueSend,
    scopes: ["structured-view"],
    title: "Send and queue",
    body: "Send a message, or queue follow-ups while the agent is still working on the last one.",
  },
  {
    id: "topbar-more",
    anchor: TOUR_ANCHORS.topbarMore,
    scopes: ["dashboard", "session", "structured-view"],
    title: "Replay this tour any time",
    body: "Reopen this walkthrough whenever you like from here: More, then Show tutorial.",
  },
] as const;

export interface ResolveTourContext {
  scope: TourScope;
  readOnly: boolean;
  isDesktop: boolean;
  cityhall?: boolean;
  /** Defaults to a `document.querySelector` probe. */
  hasAnchor?: (anchor: TourAnchorId) => boolean;
}

export function isStepEligible(
  step: TourStep,
  ctx: Pick<ResolveTourContext, "scope" | "readOnly" | "isDesktop" | "cityhall">,
): boolean {
  if (!step.scopes.includes(ctx.scope)) return false;
  if (step.writableOnly && ctx.readOnly) return false;
  if (step.desktopOnly && !ctx.isDesktop) return false;
  if (step.hiddenInCityhall && ctx.cityhall) return false;
  return true;
}

function defaultHasAnchor(anchor: TourAnchorId): boolean {
  if (typeof document === "undefined") return false;
  return document.querySelector(tourSelector(anchor)) !== null;
}

/** Metadata eligibility, then DOM presence, in TOUR_STEPS order. */
export function resolveTourSteps(ctx: ResolveTourContext): TourStep[] {
  const hasAnchor = ctx.hasAnchor ?? defaultHasAnchor;
  return TOUR_STEPS.filter((step) => {
    if (!isStepEligible(step, ctx)) return false;
    // The anchor mounts only after mid-tour navigation.
    if (step.settingsTab) return true;
    return hasAnchor(step.anchor);
  });
}

// Engine-independent tour steps. Attach anchors only via `tourAnchor()` so the drift guard in tourSteps.test.ts catches renames.

import type { ShortcutId } from "./shortcuts";

/** Values land in the DOM as `data-tour`; keep them on stable region containers. */
export const TOUR_ANCHORS = {
  topbar: "topbar",
  topbarMore: "topbar-more",
  sidebar: "sidebar",
  sidebarSettings: "sidebar-settings",
  dashboardNewSession: "dashboard-new-session",
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
    title: "Search and commands",
    body: 'Find any session, setting or action by typing what you want, like "new session" or "theme".',
    shortcutHints: [{ id: "palette", verb: "opens the palette" }],
  },
  {
    id: "sidebar",
    anchor: TOUR_ANCHORS.sidebar,
    scopes: ["dashboard", "session", "structured-view"],
    title: "Your sessions",
    body: "Each session is one agent at work. The sidebar groups them by repository, or by Group if you switch it. Open one in the Terminal view or the Structured view.",
    shortcutHints: [{ id: "sidebar", verb: "toggles the sidebar" }],
  },
  {
    id: "new-session",
    anchor: TOUR_ANCHORS.dashboardNewSession,
    scopes: ["dashboard"],
    title: "Start a session",
    body: "Pick a project and an agent, and it gets to work. A scratch session skips the project and gets its own directory that AoE creates, which you can keep when you delete it.",
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
    title: "Settings",
    body: "Theme, notifications, worktrees, agent defaults, plugins and more. Each profile keeps its own sessions and setting overrides; projects can be global or per profile.",
    shortcutHints: [{ id: "settings", verb: "opens settings" }],
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
  /** Defaults to a `document.querySelector` probe. */
  hasAnchor?: (anchor: TourAnchorId) => boolean;
}

export function isStepEligible(
  step: TourStep,
  ctx: Pick<ResolveTourContext, "scope" | "readOnly" | "isDesktop">,
): boolean {
  if (!step.scopes.includes(ctx.scope)) return false;
  if (step.writableOnly && ctx.readOnly) return false;
  if (step.desktopOnly && !ctx.isDesktop) return false;
  return true;
}

function defaultHasAnchor(anchor: TourAnchorId): boolean {
  if (typeof document === "undefined") return false;
  return document.querySelector(tourSelector(anchor)) !== null;
}

/** Metadata eligibility, then DOM presence, in TOUR_STEPS order. */
export function resolveTourSteps(ctx: ResolveTourContext): TourStep[] {
  const hasAnchor = ctx.hasAnchor ?? defaultHasAnchor;
  return TOUR_STEPS.filter((step) => isStepEligible(step, ctx) && hasAnchor(step.anchor));
}

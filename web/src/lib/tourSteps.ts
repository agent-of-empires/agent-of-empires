// Engine-independent tour steps. Attach anchors only via `tourAnchor()` so the drift guard in tourSteps.test.ts catches renames.

import type { InstallHint } from "./platform";
import type { ShortcutId } from "./shortcuts";

/** Values land in the DOM as `data-tour`; keep them on stable region containers. */
export const TOUR_ANCHORS = {
  topbar: "topbar",
  topbarMore: "topbar-more",
  sidebar: "sidebar",
  sidebarSettings: "sidebar-settings",
  dashboardNewSession: "dashboard-new-session",
  diffToggle: "diff-toggle",
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
  /** Adds the install and notifications steps when the page is a phone's browser tab. */
  installHint?: true;
}

/** One platform's notification steps, shared by the install banner, Settings › Notifications and the tour. */
export interface InstallSteps {
  /** The banner's text after "Get notifications on this device." For `push`, Settings › Notifications is its link. */
  banner: { path?: string; text: string };
  /** The steps Settings › Notifications lists; `push` has none, since its Enable button is the step. */
  settings: readonly string[];
  /** The tour's last-step line. */
  tour: string;
}

export const INSTALL_STEPS: Record<InstallHint, InstallSteps> = {
  ios: {
    banner: { path: "Share › Add to Home Screen", text: "Open AoE, sign in, and turn them on in Settings." },
    settings: [
      "Tap Share › Add to Home Screen.",
      "Open AoE from your Home Screen and sign in.",
      "Come back here and tap Enable notifications.",
    ],
    tour: "For notifications, tap Share › Add to Home Screen, open AoE, sign in, and turn them on in Settings.",
  },
  push: {
    banner: { text: "Turn them on in Settings › Notifications." },
    settings: [],
    tour: "For notifications, turn them on in Settings › Notifications.",
  },
  other: {
    banner: { text: "Install AoE from your browser's menu. Open AoE, sign in, and turn them on in Settings." },
    settings: [
      "Install AoE from your browser's menu.",
      "Open AoE and sign in.",
      "Come back here and tap Enable notifications.",
    ],
    tour: "For notifications, install AoE from your browser's menu, open it, sign in, and turn them on in Settings.",
  },
};

/** Skips inert retained views, whose hidden anchors would otherwise match first and end the tour. */
export function tourSelector(anchor: TourAnchorId): string {
  return `[data-tour="${anchor}"]:not([inert] *)`;
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
    anchor: TOUR_ANCHORS.diffToggle,
    scopes: ["session", "structured-view"],
    title: "Diff and review",
    body: "Open the diff here to review the agent's file changes and send comments back without leaving the session.",
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
    installHint: true,
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

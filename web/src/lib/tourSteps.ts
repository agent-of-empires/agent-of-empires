// Declarative source of truth for the first-run interactive tutorial.
//
// This module is intentionally framework/engine-independent: it knows nothing
// about react-joyride. Steps are described as plain data keyed by typed anchor
// ids, and a single `tourAnchor()` helper is the only sanctioned way to attach
// an anchor to a DOM node. The CI drift guard (tourSteps.test.ts) relies on that
// convention: every anchor must be referenced through `TOUR_ANCHORS.<key>`, never
// as a raw `data-tour="..."` literal, so a renamed or deleted anchor fails fast.

import type { ShortcutId } from "./shortcuts";

/**
 * Every UI region the tour can point at. The string value is the literal that
 * lands in the DOM as `data-tour="<value>"`. Keep these on stable region
 * containers, not on volatile rows or buttons that come and go per render.
 */
export const TOUR_ANCHORS = {
  topbar: "topbar",
  topbarMore: "topbar-more",
  sidebar: "sidebar",
  sidebarSettings: "sidebar-settings",
  dashboardNewSession: "dashboard-new-session",
  settingsWorktree: "settings-worktree",
  settingsPlugins: "settings-plugins",
  rightPanel: "right-panel",
  composer: "acp-composer",
  modePicker: "acp-mode-picker",
  queueSend: "acp-queue-send",
} as const;

export type TourAnchorId = (typeof TOUR_ANCHORS)[keyof typeof TOUR_ANCHORS];

/**
 * The dashboard, a terminal session, and a structured view session mount mutually
 * exclusive UI. A step declares which scopes it belongs to; the resolver never
 * shows a step outside its scope, so a missing anchor is only ever "legitimately
 * absent on this view", never a silently swallowed regression.
 */
export type TourScope = "dashboard" | "session" | "structured-view";

/**
 * A tour shortcut hint references a registered shortcut by id (so the rendered
 * key chord cannot drift from the actual binding) plus a step-local verb phrase.
 * TourRunner renders it as `${formatTourShortcut(chord)} ${verb}`.
 */
export interface TourShortcutHint {
  id: ShortcutId;
  verb: string;
}

export interface TourStep {
  /** Stable id, also used as the react-joyride step id. */
  id: string;
  anchor: TourAnchorId;
  scopes: readonly TourScope[];
  title: string;
  body: string;
  /** Shortcut hints rendered under the body, resolved from the SHORTCUTS registry. */
  shortcutHints?: readonly TourShortcutHint[];
  /** Drop the step when the dashboard is in read-only mode (mutation UI absent). */
  writableOnly?: boolean;
  /** Drop the step on coarse-pointer / non-desktop layouts (region not shown). */
  desktopOnly?: boolean;
  /**
   * The step lives inside the route-driven Settings modal. Presence means two
   * things: the anchor is deferred (it only mounts once the tour navigates to
   * this tab, so resolveTourSteps skips the launch-time DOM probe), and the
   * runner must navigate to `/settings/<tab>` before showing the step and close
   * settings when leaving it. TourRunner drives this in controlled mode so the
   * target is mounted before react-joyride evaluates it (never a race).
   */
  settingsTab?: "worktree" | "plugins";
}

/** The CSS selector that resolves a given anchor in the DOM. */
export function tourSelector(anchor: TourAnchorId): string {
  return `[data-tour="${anchor}"]`;
}

/**
 * The only sanctioned way to attach a tour anchor to a JSX element. Spread it:
 * `<div {...tourAnchor(TOUR_ANCHORS.sidebar)} />`. Using this instead of a raw
 * `data-tour="..."` string keeps the CI drift guard honest.
 */
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
  /**
   * Whether the anchor currently resolves in the DOM. Defaults to a
   * `document.querySelector` probe; injectable for tests. Steps eligible by
   * metadata but absent from the DOM are dropped (defense in depth on top of the
   * scope filter), so the engine never points at a node that is not painted.
   */
  hasAnchor?: (anchor: TourAnchorId) => boolean;
}

/** Whether a step is eligible by metadata alone (ignores DOM presence). */
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

/**
 * The steps to actually run for the current view: scope/read-only/desktop
 * eligibility first, then DOM presence. Order follows TOUR_STEPS.
 */
export function resolveTourSteps(ctx: ResolveTourContext): TourStep[] {
  const hasAnchor = ctx.hasAnchor ?? defaultHasAnchor;
  return TOUR_STEPS.filter((step) => {
    if (!isStepEligible(step, ctx)) return false;
    // settingsTab steps live behind the Settings route; their anchor only mounts
    // once the runner navigates there mid-tour, so the launch-time DOM probe
    // would wrongly drop them. Eligibility (scope/read-only/desktop) still gates.
    if (step.settingsTab) return true;
    return hasAnchor(step.anchor);
  });
}

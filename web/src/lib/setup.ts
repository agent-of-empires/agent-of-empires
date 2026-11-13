// Setup wizard policy: step copy, progress counts, and when to redirect or show the dashboard banner.
import type { SetupStatus, SetupStep, SetupStepKey } from "./api";

export interface SetupStepInfo {
  title: string;
  lead: string;
  /** Where the step's settings live outside the wizard. */
  link: string;
  linkLabel: string;
}

export const SETUP_STEP_INFO: Record<SetupStepKey, SetupStepInfo> = {
  theme_telemetry: {
    title: "Theme and telemetry",
    lead: "Pick how AoE looks, and whether it may send anonymous usage counts.",
    link: "/settings/theme",
    linkLabel: "Settings › Theme",
  },
  agents: {
    title: "Agents",
    lead: "AoE runs coding Agents for you. Pick the one you use most.",
    link: "/settings/session",
    linkLabel: "Settings › Session",
  },
  project: {
    title: "First Project",
    lead: "A Project is a folder or repo your Sessions work in.",
    link: "/setup?step=project",
    linkLabel: "Setup › First Project",
  },
  worktrees: {
    title: "Worktrees",
    lead: "Give each Session its own git worktree so Agents never step on each other.",
    link: "/settings/worktree",
    linkLabel: "Settings › Worktree",
  },
  sandbox: {
    title: "Sandbox",
    lead: "Run Sessions in a container so Agents can't touch the rest of your machine.",
    link: "/settings/sandbox",
    linkLabel: "Settings › Sandbox",
  },
  notifications: {
    title: "Notifications and access",
    lead: "Hear about Agents that need you, and see how this dashboard is reached.",
    link: "/settings/notifications",
    linkLabel: "Settings › Notifications",
  },
};

/** Done for the wizard rail: configured, or deliberately skipped. */
export const stepResolved = (step: SetupStep) => step.configured || step.skipped;

export const configuredCount = (steps: SetupStep[]) => steps.filter((s) => s.configured).length;
export const resolvedCount = (steps: SetupStep[]) => steps.filter(stepResolved).length;

/** Where the wizard opens: the first open step, else the first step. */
export function firstOpenIndex(steps: SetupStep[]): number {
  return Math.max(
    0,
    steps.findIndex((s) => !stepResolved(s)),
  );
}

/** The first required step that is not configured, which blocks Finish. */
export const openRequiredStep = (steps: SetupStep[]) => steps.find((s) => s.required && !s.configured);

interface Viewer {
  status: SetupStatus | null;
  readOnly: boolean;
  cityhall: boolean;
  automated: boolean;
}

const canSetUp = (v: Viewer): v is Viewer & { status: SetupStatus } =>
  !!v.status && !v.readOnly && !v.cityhall && !v.automated;

/** Only the dashboard root redirects, so deep links keep working. */
export function shouldRedirectToSetup(v: Viewer & { pathname: string }): boolean {
  return v.pathname === "/" && canSetUp(v) && !v.status.finished && !v.status.deferred;
}

/** Banner rows: every step that is not configured, skipped ones included. */
export function bannerSteps(v: Viewer): SetupStep[] {
  if (!canSetUp(v) || v.status.banner_dismissed) return [];
  return v.status.steps.filter((s) => !s.configured);
}

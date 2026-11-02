import type { SessionStatus } from "./types";
import { STATUS_TEXT_CLASS } from "./session";

/** One meaning of a status glyph, shared by the sidebar legend and the glyph's tooltip. */
export interface StatusMeaning {
  label: string;
  description: string;
}

/** What each status means, in glossary terms. Deleting reads as Error, which it shows as. */
const STATUS_MEANING: Record<SessionStatus, StatusMeaning> = {
  Running: { label: "Running", description: "the Agent is working" },
  Waiting: { label: "Waiting", description: "the Agent needs your input" },
  Starting: { label: "Starting", description: "the Session is starting up" },
  Creating: { label: "Creating", description: "the Session is being created" },
  Idle: { label: "Idle", description: "the Agent is ready for your next message" },
  Stopped: { label: "Stopped", description: "the Session isn't running" },
  Unknown: { label: "Unknown", description: "AoE can't tell this Session's state yet" },
  Error: { label: "Error", description: "the Session failed; also shown while deleting" },
  Deleting: { label: "Error", description: "the Session failed; also shown while deleting" },
};
const FRESH_IDLE_MEANING: StatusMeaning = { label: "Idle, just finished", description: "the Agent finished recently" };
const DORMANT_MEANING: StatusMeaning = {
  label: "Dormant",
  description: "paused to save resources; wakes when you send",
};

/** What a Session's glyph means right now, for its tooltip. */
export function statusMeaning(status: SessionStatus, { fresh = false, dormant = false } = {}): StatusMeaning {
  if (dormant) return DORMANT_MEANING;
  if (fresh && status === "Idle") return FRESH_IDLE_MEANING;
  return STATUS_MEANING[status];
}

/** One legend row: the statuses it stands for, how to draw its glyph, and what it means. */
export interface StatusLegendRow extends StatusMeaning {
  statuses: SessionStatus[];
  variant: "status" | "fresh" | "dormant";
  colorClass: string;
}

/**
 * The sidebar legend's status rows, generated from the tables above in the order the legend
 * shows them. Every status lands in exactly one row; Deleting shares Error's.
 */
export function statusLegend(): StatusLegendRow[] {
  const row = (status: SessionStatus, extra: SessionStatus[] = []): StatusLegendRow => ({
    ...STATUS_MEANING[status],
    statuses: [status, ...extra],
    variant: "status",
    colorClass: STATUS_TEXT_CLASS[status],
  });
  return [
    row("Running"),
    row("Waiting"),
    row("Starting"),
    row("Creating"),
    { ...FRESH_IDLE_MEANING, statuses: [], variant: "fresh", colorClass: "text-status-fresh-idle" },
    row("Idle"),
    { ...DORMANT_MEANING, statuses: [], variant: "dormant", colorClass: "text-status-dormant" },
    row("Stopped"),
    row("Unknown"),
    row("Error", ["Deleting"]),
  ];
}

import { sessionFlagGate } from "./sessionFlagGate";

/** Mirrors `crate::usage::UsageSummary`. */
export interface UsageSummary {
  resets: number;
  clears: number;
  compactions: number;
  compactionsAuto: number;
  compactionsManual: number;
  resumes: number;
  prompts: number;
  turns: number;
  turnErrors: number;
  contextStartedAt: string | null;
  contextPrompts: number;
  contextTurns: number;
  trackedSince: string | null;
  lastEventAt: string | null;
  tracked: boolean;
}

/** On by default, matching `session.show_usage_overlay`. */
const gate = sessionFlagGate("show_usage_overlay", true);
export const UsageOverlayEnabledContext = gate.Context;
export const parseUsageOverlayEnabled = gate.parse;
export const useUsageOverlayEnabled = gate.use;

export { fetchSessionUsage } from "./api";

/** Mirrors the TUI's `format_duration_short`. */
export function formatDurationShort(ms: number): string {
  const minutes = Math.floor(ms / 60_000);
  if (minutes < 1) return "<1m";
  if (minutes < 60) return `${minutes}m`;
  if (minutes < 24 * 60) return `${Math.floor(minutes / 60)}h${minutes % 60}m`;
  return `${Math.floor(minutes / (24 * 60))}d${Math.floor(minutes / 60) % 24}h`;
}

/** Mirrors the TUI's `overlay_lines`. */
export function usageLines(summary: UsageSummary, createdAt: string, now: number): string[] {
  const ctx = summary.contextStartedAt ? formatDurationShort(now - Date.parse(summary.contextStartedAt)) : "?";
  return [
    `clr ${summary.clears} cmp ${summary.compactions}/${summary.compactionsAuto}a rsm ${summary.resumes}`,
    `${ctx} ${summary.contextPrompts}p ${summary.contextTurns}t age ${formatDurationShort(now - Date.parse(createdAt))}`,
  ];
}

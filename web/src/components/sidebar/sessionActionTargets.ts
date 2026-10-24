import type { SessionResponse } from "../../lib/types";
import { effectiveArchivedOf, type OptimisticTriage } from "../../lib/sidebarOptimistic";

/** Row menu actions that act on one session, so a row with several siblings must name which. */
export type SessionAction = "stop" | "start" | "fork" | "switchAgent" | "switchView" | "autoName" | "summarize";

const isStructured = (s: SessionResponse) => s.view === "structured";

const ELIGIBLE: Record<SessionAction, (s: SessionResponse, archived: boolean) => boolean> = {
  stop: (s) => !["Stopped", "Deleting", "Creating"].includes(s.status),
  // Archived and trashed sessions must be unarchived or restored before they start (#4116).
  start: (s, archived) => s.status === "Stopped" && !archived && !s.trashed_at,
  fork: (s) => isStructured(s) && !!s.acp_session_id && !!s.acp_can_fork,
  switchAgent: isStructured,
  switchView: (s) => isStructured(s) || (!!s.acp_capable && !s.structured_view_off),
  autoName: isStructured,
  summarize: isStructured,
};

export type SessionActionTargets = Record<SessionAction, SessionResponse[]>;

/** The siblings each action may act on, in row order. */
export function resolveActionTargets(sessions: SessionResponse[], optimistic: OptimisticTriage): SessionActionTargets {
  // A pending archive applies to the one session it was issued for, not every sibling.
  const archivedOf = (s: SessionResponse) =>
    s.id === optimistic.archivedSessionId ? effectiveArchivedOf(optimistic, !!s.archived_at) : !!s.archived_at;
  const targets = (action: SessionAction) => sessions.filter((s) => ELIGIBLE[action](s, archivedOf(s)));
  return {
    stop: targets("stop"),
    start: targets("start"),
    fork: targets("fork"),
    switchAgent: targets("switchAgent"),
    switchView: targets("switchView"),
    autoName: targets("autoName"),
    summarize: targets("summarize"),
  };
}

import type { SessionResponse } from "../../lib/types";
import { effectiveArchivedOf, type RowOptimistic } from "../../lib/sidebarOptimistic";

/** Row menu actions that act on one session, so a row with several siblings must name which. */
export type SessionAction =
  | "stop"
  | "start"
  | "restartUpdate"
  | "recreateContainer"
  | "fork"
  | "switchAgent"
  | "switchAccount"
  | "switchView"
  | "autoName"
  | "summarize"
  | "showOnly"
  | "openOwnWindow";

const isStructured = (s: SessionResponse) => s.view === "structured";

/** Whether a Session has a container the dashboard may recreate: the Session menu and the startup error screen. */
export const canRecreateContainer = (s: SessionResponse, archived: boolean) =>
  s.is_sandboxed && !archived && !s.trashed_at && !["Creating", "Deleting"].includes(s.status);

/** The title the startup error screen's Recreate container needs, or undefined when it isn't offered. */
export const recreateContainerTitle = (s: SessionResponse) =>
  canRecreateContainer(s, !!s.archived_at) ? s.title : undefined;

const ELIGIBLE: Record<SessionAction, (s: SessionResponse, archived: boolean) => boolean> = {
  stop: (s) => !["Stopped", "Deleting", "Creating"].includes(s.status),
  // Archived and trashed sessions must be unarchived or restored before they start (#4116).
  start: (s, archived) => s.status === "Stopped" && !archived && !s.trashed_at,
  restartUpdate: (s) => !!s.update_restart,
  recreateContainer: canRecreateContainer,
  fork: (s) => isStructured(s) && !!s.acp_session_id && !!s.acp_can_fork,
  switchAgent: isStructured,
  // A running failover is already swapping the account, so a manual restart would race it.
  switchAccount: (s, archived) => isStructured(s) && !archived && !s.trashed_at && !s.rate_limit_failover_to,
  switchView: (s) => isStructured(s) || (!!s.acp_capable && !s.structured_view_off),
  autoName: isStructured,
  summarize: isStructured,
  showOnly: (s) => !s.trashed_at,
  openOwnWindow: (s) => !s.trashed_at,
};

export type SessionActionTargets = Record<SessionAction, SessionResponse[]>;

/** The siblings each action may act on, in row order. */
export function resolveActionTargets(sessions: SessionResponse[], optimistic: RowOptimistic): SessionActionTargets {
  // A pending action applies to the one session it was issued for, not every sibling.
  const archivedOf = (s: SessionResponse) => {
    const entry = optimistic.bySession?.get(s.id) ?? (s.id === optimistic.sessionId ? optimistic : undefined);
    return entry ? effectiveArchivedOf(entry, !!s.archived_at) : !!s.archived_at;
  };
  const targets = (action: SessionAction) => sessions.filter((s) => ELIGIBLE[action](s, archivedOf(s)));
  return {
    stop: targets("stop"),
    start: targets("start"),
    restartUpdate: targets("restartUpdate"),
    recreateContainer: targets("recreateContainer"),
    fork: targets("fork"),
    switchAgent: targets("switchAgent"),
    switchAccount: targets("switchAccount"),
    switchView: targets("switchView"),
    autoName: targets("autoName"),
    summarize: targets("summarize"),
    showOnly: targets("showOnly"),
    openOwnWindow: targets("openOwnWindow"),
  };
}

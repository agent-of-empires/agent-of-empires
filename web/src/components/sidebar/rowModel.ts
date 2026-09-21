import type { SessionStatus, Workspace } from "../../lib/types";
import { getStatusTextClass, isSessionActive } from "../../lib/session";
import { workspaceAttentionCount } from "../../lib/sidebarSort";
import {
  effectiveArchivedOf,
  effectivePinnedOf,
  effectiveSnoozedUntilOf,
  effectiveUnreadOf,
  type OptimisticTriage,
} from "../../lib/sidebarOptimistic";

/** Mirrors the Rust `SESSION_COLORS` list. */
export const SESSION_COLOR_OPTIONS: { key: string; label: string; dotClass: string }[] = [
  { key: "red", label: "Red · needs attention", dotClass: "bg-red-500" },
  { key: "amber", label: "Amber · working", dotClass: "bg-amber-400" },
  { key: "green", label: "Green · done", dotClass: "bg-green-500" },
];

export type NotifyPreset = "off" | "default" | "all";

/** Mixed per-event overrides read as "default", which resets them cleanly when picked. */
function detectNotifyPreset(values: (boolean | null | undefined)[]): NotifyPreset {
  if (values.every((v) => v === false)) return "off";
  if (values.every((v) => v === true)) return "all";
  return "default";
}

/** Status shown for a workspace: its first live session, else its first errored one, else its first. */
export function bestSession(ws: Workspace, idleDecayWindowMs: number) {
  const running = ws.sessions.find((s) => isSessionActive(s, idleDecayWindowMs));
  if (running) {
    return {
      status: running.status,
      createdAt: running.created_at,
      idleEnteredAt: running.idle_entered_at ?? null,
      dormant: running.dormant,
    };
  }
  const error = ws.sessions.find((s) => s.status === "Error");
  if (error)
    return { status: "Error" as SessionStatus, createdAt: error.created_at, idleEnteredAt: null, dormant: false };
  const first = ws.sessions[0];
  return {
    status: first?.status ?? ("Unknown" as SessionStatus),
    createdAt: first?.created_at ?? null,
    idleEnteredAt: first?.idle_entered_at ?? null,
    dormant: first?.dormant ?? false,
  };
}

/** Everything a sidebar row renders or gates on, derived from the workspace plus its optimistic overlay. */
export function deriveRowModel(
  workspace: Workspace,
  optimistic: OptimisticTriage,
  {
    idleDecayWindowMs,
    isActive,
    unreadIndicatorEnabled,
  }: { idleDecayWindowMs: number; isActive: boolean; unreadIndicatorEnabled: boolean },
) {
  const best = bestSession(workspace, idleDecayWindowMs);
  const { sessions } = workspace;
  const firstSession = sessions[0];
  const runningSession = sessions.find((s) => isSessionActive(s, idleDecayWindowMs));
  const sessionTitle = firstSession?.title.trim() ?? "";
  const branchLabel = workspace.branch ?? null;
  const label =
    sessions.length === 1 ? sessionTitle || branchLabel || "default" : branchLabel || sessionTitle || "default";
  const sessionColor = sessions.map((s) => s.color).find((c) => c != null) ?? null;
  const isPinned = sessions.some((s) => s.pinned_at != null);
  const isArchived = sessions.some((s) => s.archived_at != null);
  const snoozedUntil = sessions.find((s) => s.snoozed_until)?.snoozed_until ?? null;
  const effectiveSnoozedUntil = effectiveSnoozedUntilOf(optimistic, snoozedUntil);
  const effectiveArchived = effectiveArchivedOf(optimistic, isArchived);
  const effectiveUnread = effectiveUnreadOf(
    optimistic,
    sessions.some((s) => s.unread === true),
  );
  // The open row and sunk rows hide the marker; the stored flag survives for when they resurface.
  const isUnread =
    unreadIndicatorEnabled && effectiveUnread && !isActive && !effectiveArchived && effectiveSnoozedUntil == null;
  const status = best.status;
  const needsAttention = workspaceAttentionCount(workspace) > 0;
  return {
    ...best,
    firstSession,
    runningSession,
    navigationSession: runningSession ?? firstSession,
    acpSession: sessions.find((s) => s.view === "structured"),
    sessionId: firstSession?.id,
    sessionTitle,
    branchLabel,
    label,
    newSessionRepoPath: firstSession?.main_repo_path || firstSession?.project_path || null,
    textClass: getStatusTextClass(
      { status, idle_entered_at: best.idleEnteredAt, dormant: best.dormant },
      idleDecayWindowMs,
    ),
    isFavorited: sessions.some((s) => s.favorited),
    sessionColor,
    sessionColorDot: SESSION_COLOR_OPTIONS.find((o) => o.key === sessionColor)?.dotClass ?? null,
    isPinned,
    isArchived,
    effectivePinned: effectivePinnedOf(optimistic, isPinned),
    effectiveArchived,
    effectiveSnoozedUntil,
    effectiveSnoozed: effectiveSnoozedUntil != null,
    effectiveUnread,
    // The unread dot replaces only a resting glyph; live status outranks it.
    showUnreadGlyph: isUnread && (status === "Idle" || status === "Unknown"),
    needsAttention,
    attentionHint:
      status === "Waiting"
        ? "waiting for your input"
        : status === "Error"
          ? "needs attention (error)"
          : "needs your attention",
    isDeleting: status === "Deleting",
    notifyPreset: detectNotifyPreset([
      firstSession?.notify_on_waiting,
      firstSession?.notify_on_idle,
      firstSession?.notify_on_error,
    ]),
    // Moving the worktree is only safe for a managed, stopped, untied session.
    canEditWorkdir:
      !!firstSession?.has_managed_worktree &&
      !firstSession?.tie_workdir_to_name &&
      !runningSession &&
      !!firstSession?.id,
    // Mirrors the refusals in `attach_project::plan`; Running/Waiting are decided server-side.
    canAddProject:
      !firstSession?.scratch &&
      !firstSession?.archived_at &&
      !firstSession?.trashed_at &&
      firstSession?.status !== "Creating" &&
      firstSession?.status !== "Deleting",
    canStop: !["Stopped", "Deleting", "Creating"].includes(status),
    canStart: status === "Stopped",
  };
}

export type RowModel = ReturnType<typeof deriveRowModel>;

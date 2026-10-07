import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Archive, Hourglass, Moon, Pencil, Sparkles } from "lucide-react";
import type {
  ContextResumeAvailability,
  ContextResumeUnavailableReason,
  SessionResponse,
  Workspace,
} from "../../lib/types";
import { useHasDraftForSessions } from "../../lib/acpDrafts";
import { useQueuedCountForSessions } from "../../hooks/useAcpQueueCount";
import { summarizeRateLimits } from "../../lib/rateLimitSummary";
import { computeSessionRowTag, useSessionRowTagMode } from "../../lib/sessionRowTag";
import { PluginRowLine } from "../plugin/PluginSlots";
import { formatDurationSecondsShort, formatSnoozeRemainingShort } from "./format";
import type { RowModel } from "./rowModel";

const CHIP = "inline-flex shrink-0 items-center rounded border px-1 py-0 text-[10px]";
const MUTED = "border-surface-700/40 bg-surface-800/40 font-mono font-medium text-text-dim";
const AMBER = "border-amber-700/40 bg-amber-950/30 font-medium text-amber-300";

const CONTEXT_RESUME_UNAVAILABLE_DETAIL: Record<ContextResumeUnavailableReason, string> = {
  agent_unsupported: "this agent has no verified native resume path",
  sandbox_unsupported: "the sandbox has no authoritative resume target",
  command_unsupported: "the launch command hides the agent behind a launcher",
  forced_fresh: "the next launch was explicitly reset",
  invalid_target: "the saved resume target is invalid",
  fork_pending: "the requested fork has not completed",
  previous_failure: "the previous resume attempt failed",
  no_target: "no resume target has been captured",
};

function Chip({
  title,
  label,
  className,
  children,
}: {
  title: string;
  label: string;
  className: string;
  children: ReactNode;
}) {
  return (
    <span title={title} aria-label={label} className={className}>
      {children}
    </span>
  );
}

const Pulse = () => <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-amber-400/80" />;

function ContextResumeBadge({ availability }: { availability: ContextResumeAvailability | undefined }) {
  if (availability?.state !== "unavailable") return null;
  const detail = CONTEXT_RESUME_UNAVAILABLE_DETAIL[availability.reason];
  return (
    <span
      title={detail ? `Context resume unavailable: ${detail}` : "Context resume unavailable"}
      className={`${CHIP} border-status-warning/40 bg-status-warning/10 font-mono font-medium text-text-primary`}
    >
      ctx:no
    </span>
  );
}

/** Ticks once a second until the wake time, then reads "waking…" until the next poll clears it. */
function WakeupCountdown({ wakeAt, reason }: { wakeAt: string; reason: string | null | undefined }) {
  const targetMs = Date.parse(wakeAt);
  const [now, setNow] = useState(() => Date.now());
  const elapsed = !Number.isFinite(targetMs) || targetMs <= now;
  useEffect(() => {
    if (elapsed) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [elapsed]);
  if (!Number.isFinite(targetMs)) return null;
  const remaining = Math.max(0, Math.floor((targetMs - now) / 1000));
  const label = elapsed ? "waking…" : `in ${formatDurationSecondsShort(remaining)}`;
  return (
    <Chip
      title={reason ? `Scheduled wakeup: ${reason}` : "Scheduled wakeup"}
      label={`Scheduled wakeup ${label}`}
      className={`${CHIP} gap-0.5 border-sky-700/40 bg-sky-950/30 font-medium text-sky-300`}
    >
      <span aria-hidden="true">⏰</span>
      {label}
    </Chip>
  );
}

function RateLimitBadge({ sessions }: { sessions: SessionResponse[] }) {
  const rateLimited = useMemo(() => summarizeRateLimits(sessions), [sessions]);
  if (!rateLimited) return null;
  const reset = rateLimited.resetsAt ? new Date(rateLimited.resetsAt) : null;
  const resetLabel =
    reset && !Number.isNaN(reset.getTime())
      ? reset.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
      : null;
  const title = `Rate-limited${rateLimited.count > 1 ? ` (${rateLimited.count} sessions)` : ""}${resetLabel ? `; resets at ${resetLabel}` : ""}`;
  return (
    <Chip
      title={title}
      label={title}
      className={`${CHIP} gap-0.5 border-orange-700/40 bg-orange-950/30 font-mono font-medium text-orange-300`}
    >
      <Hourglass className="h-3 w-3" />
      {rateLimited.count > 1 && <span className="tabular-nums">{rateLimited.count}</span>}
      {resetLabel && <span>{resetLabel}</span>}
    </Chip>
  );
}

/** Badges after the row label; hidden in the compact rail. */
export function RowTrailingBadges({ workspace, model }: { workspace: Workspace; model: RowModel }) {
  const { firstSession: first, effectiveArchived, effectiveSnoozedUntil } = model;
  const rowTag = computeSessionRowTag(workspace, useSessionRowTagMode());
  const baseBranch = first?.base_branch ?? null;
  const sessionIds = useMemo(() => workspace.sessions.map((s) => s.id), [workspace.sessions]);
  const hasDraft = useHasDraftForSessions(sessionIds);
  const queuedCount = useQueuedCountForSessions(sessionIds);
  const worker = first?.view === "structured" ? first.acp_worker_state : undefined;
  return (
    <>
      {rowTag && (
        <span
          data-testid="sidebar-session-row-tag"
          title={rowTag.kind === "branch" && baseBranch ? `${rowTag.title} (based on ${baseBranch})` : rowTag.title}
          className={`${CHIP} font-mono font-medium ${
            rowTag.kind === "branch"
              ? "border-brand-700/40 bg-brand-700/5 text-brand-300"
              : "border-surface-700/40 bg-surface-800/40 text-text-dim"
          }`}
        >
          [{rowTag.content}]
        </span>
      )}
      {hasDraft && (
        <span title="Unsent draft" aria-label="Unsent draft" className="inline-flex shrink-0">
          <Pencil className="h-3 w-3 text-amber-400/90" />
        </span>
      )}
      {queuedCount > 0 && (
        <Chip
          title={`${queuedCount} queued prompt${queuedCount === 1 ? "" : "s"}`}
          label={`${queuedCount} queued`}
          className={`${CHIP} border-sky-700/40 bg-sky-950/30 font-mono font-medium tabular-nums text-sky-300`}
        >
          {queuedCount}
        </Chip>
      )}
      <RateLimitBadge sessions={workspace.sessions} />
      {effectiveArchived && (
        <Chip title="Archived" label="Archived" className={`${CHIP} gap-0.5 ${MUTED}`}>
          <Archive className="h-3 w-3" />
          <span className="hidden sm:inline">archived</span>
        </Chip>
      )}
      {!effectiveArchived && effectiveSnoozedUntil && (
        <Chip
          title={`Snoozed until ${new Date(effectiveSnoozedUntil).toLocaleString()}`}
          label="Snoozed"
          className={`${CHIP} gap-0.5 ${MUTED}`}
        >
          <Moon className="h-3 w-3" />
          <span>{formatSnoozeRemainingShort(effectiveSnoozedUntil)}</span>
        </Chip>
      )}
      <ContextResumeBadge availability={model.navigationSession?.context_resume} />
      {worker === "resuming" && (
        <Chip title="Structured view worker is resuming" label="Resuming" className={`${CHIP} gap-0.5 ${AMBER}`}>
          <Pulse />
          Resuming
        </Chip>
      )}
      {worker === "stopping" && (
        <Chip
          title="Structured view worker is stopping; it will not resume until the process has exited"
          label="Stopping"
          className={`${CHIP} gap-0.5 ${AMBER}`}
        >
          <Pulse />
          Stopping
        </Chip>
      )}
      {first?.smart_rename === "pending" && (
        <Chip
          title="Will auto-name this session from your first message"
          label="Will auto-name"
          className={`${CHIP} gap-0.5 ${MUTED}`}
        >
          <Sparkles className="h-3 w-3" />
          <span className="hidden sm:inline">Auto-name</span>
        </Chip>
      )}
      {first?.smart_rename === "running" && (
        <Chip title="Generating a name from your first message" label="Naming" className={`${CHIP} gap-0.5 ${AMBER}`}>
          <Pulse />
          Naming…
        </Chip>
      )}
      {first?.next_wakeup_at && <WakeupCountdown wakeAt={first.next_wakeup_at} reason={first.next_wakeup_reason} />}
      {first?.monitor_active && (
        <Chip
          title={first.monitor_description ? `Monitoring: ${first.monitor_description}` : "Monitoring a background job"}
          label={`Monitoring${first.monitor_description ? ` ${first.monitor_description}` : ""}`}
          className={`${CHIP} gap-0.5 border-violet-700/40 bg-violet-950/30 font-medium text-violet-300`}
        >
          <span aria-hidden="true">👁</span>
          monitoring
        </Chip>
      )}
    </>
  );
}

function PlanProgressMini({ summary }: { summary: NonNullable<SessionResponse["plan_summary"]> }) {
  const pct = summary.total > 0 ? Math.min(100, Math.round((summary.completed / summary.total) * 100)) : 0;
  const steps = `Plan progress: ${summary.completed} of ${summary.total} steps`;
  return (
    <div className="mt-1 flex items-center gap-2" title={summary.current_step_title ?? "plan in progress"}>
      <div
        role="progressbar"
        aria-valuenow={summary.completed}
        aria-valuemin={0}
        aria-valuemax={summary.total}
        aria-label={summary.current_step_title ? `${steps}; current step ${summary.current_step_title}` : steps}
        className="h-1 flex-1 rounded-full bg-surface-800 overflow-hidden"
      >
        <div className="h-full bg-brand-400 transition-all" style={{ width: `${pct}%` }} />
      </div>
      <span className="text-[10px] font-mono tabular-nums text-text-dim shrink-0">
        {summary.completed}/{summary.total}
      </span>
    </div>
  );
}

/** Plugin line, plan progress, and multi-repo chips below the label; hidden in the compact rail. */
export function RowSubRows({ first }: { first: SessionResponse | undefined }) {
  if (!first) return null;
  const plan = first.plan_summary;
  // A finished plan on an idle session is static clutter; it returns with the next prompt.
  const showPlan = plan && plan.total > 0 && !(plan.completed >= plan.total && first.status === "Idle");
  return (
    <>
      <PluginRowLine sessionId={first.id} />
      {showPlan && <PlanProgressMini summary={plan} />}
      {(first.workspace_repos?.length ?? 0) > 1 && (
        <span
          className="mt-0.5 flex flex-wrap gap-1 text-[10px] font-mono text-text-dim"
          title={first.workspace_repos.map((r) => r.source_path).join("\n")}
        >
          {first.workspace_repos.map((r) => (
            <span
              key={r.source_path}
              className="px-1 py-px bg-surface-800/50 border border-surface-700/40 rounded text-text-secondary"
            >
              {r.name}
            </span>
          ))}
        </span>
      )}
    </>
  );
}

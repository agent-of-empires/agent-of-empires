import { useEffect, useState } from "react";

import { useRespawnSession } from "../../hooks/useRespawnSession";
import type { AcpState } from "../../lib/acpTypes";
import { StartupErrorBanner } from "./StartupErrorBanner";
import { pickWorkerStoppedVariant, showWorkerStoppingBanner } from "./workerStoppedBanner";

/** Worker lifecycle and triage banners stacked above the transcript. */
export function SessionBanners({
  sessionId,
  state,
  acpWorkerState,
  trashedAt,
  archivedAt,
  snoozedUntil,
  onRestore,
  dismissError,
}: {
  sessionId: string;
  state: AcpState;
  acpWorkerState: "absent" | "resuming" | "running" | "stopping";
  trashedAt: string | null;
  archivedAt: string | null;
  snoozedUntil: string | null;
  onRestore?: () => Promise<boolean> | void;
  dismissError: () => void;
}) {
  const variant = pickWorkerStoppedVariant({
    workerStopped: state.workerStopped,
    startupError: state.startupError,
    trashedAt,
    archivedAt,
    snoozedUntil,
    workerStopping: acpWorkerState === "stopping",
  });
  const healthy = !state.startupError && !state.workerStopped && !state.workerRestarting;
  const idle = healthy && !state.turnActive;
  return (
    <>
      {state.startupError && <StartupErrorBanner sessionId={sessionId} message={state.startupError} />}
      {variant === "trashed" && <TrashedWorkerStoppedBanner sessionId={sessionId} onRestore={onRestore} />}
      {variant === "archived" && <ArchivedWorkerStoppedBanner sessionId={sessionId} />}
      {variant === "snoozed" && snoozedUntil && (
        <SnoozedWorkerStoppedBanner sessionId={sessionId} snoozedUntil={snoozedUntil} />
      )}
      {variant === "generic" && <WorkerStoppedBanner sessionId={sessionId} />}
      {state.workerRestarting && !state.startupError && !state.workerStopped && (
        <WorkerRestartingBanner agentUnresponsive={state.agentUnresponsive} agentOrphaned={state.agentOrphaned} />
      )}
      {acpWorkerState === "resuming" &&
        healthy &&
        (state.lastSeq === 0 ? (
          <PulseBanner>Starting structured view worker for new session… this can take a few seconds.</PulseBanner>
        ) : (
          <PulseBanner>
            Resuming structured view worker… cached transcript still available. Queued prompts will send once the agent
            is back online.
          </PulseBanner>
        ))}
      {showWorkerStoppingBanner({ acpWorkerState, startupError: state.startupError }) && <WorkerStoppingBanner />}
      {state.nextWakeupAt && idle && (
        <ScheduledWakeupBanner wakeAt={state.nextWakeupAt} reason={state.nextWakeupReason} />
      )}
      {state.monitorArmed && !state.nextWakeupAt && idle && (
        <ChipBanner tone="violet" icon="👁" detail={state.monitorDescription}>
          Monitoring a background job
        </ChipBanner>
      )}
      {state.lastError && <InteractionErrorBanner message={state.lastError} onDismiss={dismissError} />}
    </>
  );
}

const PULSE_TONES = {
  warning: ["border-status-warning/30 bg-status-warning/10 text-status-warning", "bg-status-warning"],
  sky: ["border-sky-900/60 bg-sky-950/40 text-sky-200", "bg-sky-400"],
} as const;

function PulseBanner({ tone = "warning", children }: { tone?: keyof typeof PULSE_TONES; children: React.ReactNode }) {
  const [box, dot] = PULSE_TONES[tone];
  return (
    <div className={`flex items-center gap-2 border-b ${box} px-4 py-2 text-xs`}>
      <span className={`inline-block h-2 w-2 animate-pulse rounded-full ${dot}`} aria-hidden />
      <span>{children}</span>
    </div>
  );
}

const CHIP_TONES = {
  sky: ["border-sky-900/60 bg-sky-950/40 text-sky-200", "text-sky-300/70"],
  violet: ["border-violet-900/60 bg-violet-950/40 text-violet-200", "text-violet-300/70"],
} as const;

function ChipBanner({
  tone,
  icon,
  detail,
  children,
}: {
  tone: keyof typeof CHIP_TONES;
  icon: string;
  detail: string | null;
  children: React.ReactNode;
}) {
  const [box, dim] = CHIP_TONES[tone];
  return (
    <div className={`flex items-center gap-2 border-b ${box} px-4 py-2 text-xs`}>
      <span aria-hidden className="text-base leading-none">
        {icon}
      </span>
      <span className="truncate">
        {children}
        {detail ? <span className={dim}>: {detail}</span> : null}
      </span>
    </div>
  );
}

function InteractionErrorBanner({ message, onDismiss }: { message: string; onDismiss: () => void }) {
  return (
    <div className="flex items-start justify-between gap-3 border-b border-status-warning/30 bg-status-warning/10 px-4 py-2 text-status-warning">
      <div className="flex-1 min-w-0">
        <div className="text-xs font-medium">Action did not complete</div>
        <div className="mt-0.5 text-xs text-status-warning/90 break-words">{message}</div>
      </div>
      <button
        type="button"
        onClick={onDismiss}
        className="shrink-0 rounded-md border border-status-warning/40 bg-status-warning/20 px-2 py-1 text-[10px] font-mono uppercase tracking-wide text-status-warning hover:bg-status-warning/30"
      >
        Dismiss
      </button>
    </div>
  );
}

/** Orphaned (turn finished, no PromptResponse) wins over unresponsive (ignored cancel). */
export function WorkerRestartingBanner({
  agentUnresponsive,
  agentOrphaned,
}: {
  agentUnresponsive: boolean;
  agentOrphaned: boolean;
}) {
  return (
    <PulseBanner tone="sky">
      {agentOrphaned
        ? "Agent finished but didn't notify the daemon. Restarting worker; your transcript will be preserved."
        : agentUnresponsive
          ? "Agent stopped responding to cancel. Restarting worker; your transcript will be preserved."
          : "Restarting structured view worker… the daemon will respawn the agent with your existing transcript shortly."}
    </PulseBanner>
  );
}

function WorkerStoppingBanner() {
  return (
    <PulseBanner>
      Stopping structured view worker… waiting for the agent process to exit before anything can resume.
    </PulseBanner>
  );
}

// A real wake flips `turnActive` within seconds, so this only clears a stale
// banner left by a fallback wakeup that its primary signal superseded.
const WAKING_GRACE_MS = 10_000;

export function ScheduledWakeupBanner({ wakeAt, reason }: { wakeAt: string; reason: string | null }) {
  const targetMs = Date.parse(wakeAt);
  const [now, setNow] = useState(() => Date.now());
  const [dismissed, setDismissed] = useState(false);
  const elapsed = !Number.isFinite(targetMs) || targetMs <= now;
  // A fresh wake reuses this instance; un-dismiss during render.
  const [prevWakeAt, setPrevWakeAt] = useState(wakeAt);
  if (wakeAt !== prevWakeAt) {
    setPrevWakeAt(wakeAt);
    setDismissed(false);
  }
  useEffect(() => {
    if (elapsed) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [elapsed]);
  useEffect(() => {
    if (!elapsed) return;
    const id = setTimeout(() => setDismissed(true), WAKING_GRACE_MS);
    return () => clearTimeout(id);
  }, [elapsed]);
  if (!Number.isFinite(targetMs) || dismissed) return null;
  const remaining = Math.max(0, Math.floor((targetMs - now) / 1000));
  const pad = (n: number) => String(n).padStart(2, "0");
  const wakeDate = new Date(targetMs);
  const clock = `${pad(wakeDate.getHours())}:${pad(wakeDate.getMinutes())}`;
  const inText =
    remaining < 60
      ? `${remaining}s`
      : remaining < 3600
        ? `${Math.floor(remaining / 60)}m ${pad(remaining % 60)}s`
        : `${Math.floor(remaining / 3600)}h ${Math.floor((remaining % 3600) / 60)}m`;
  return (
    <ChipBanner tone="sky" icon="⏰" detail={reason}>
      {elapsed ? "Waking…" : `Asleep until ${clock} (in ${inText})`}
    </ChipBanner>
  );
}

const WARNING_BUTTON =
  "shrink-0 rounded-md border border-status-warning/40 bg-status-warning/20 px-3 py-1 text-xs font-medium text-status-warning hover:bg-status-warning/30 disabled:cursor-not-allowed disabled:opacity-60";

function StoppedPanel({
  testId,
  title,
  action,
  footer,
  children,
}: {
  testId?: string;
  title: string;
  action?: React.ReactNode;
  footer?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div
      className="border-b border-status-warning/30 bg-status-warning/10 px-4 py-3 text-status-warning"
      data-testid={testId}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="flex-1 min-w-0">
          <div className="text-sm font-medium">{title}</div>
          <div className="mt-1 text-xs text-status-warning/90">{children}</div>
        </div>
        {action}
      </div>
      {footer}
    </div>
  );
}

function WorkerStoppedBanner({ sessionId }: { sessionId: string }) {
  const { state: retryState, error: retryError, respawn: handleReconnect } = useRespawnSession(sessionId);
  return (
    <StoppedPanel
      title="Structured view worker stopped"
      action={
        <button type="button" onClick={handleReconnect} disabled={retryState === "retrying"} className={WARNING_BUTTON}>
          {retryState === "retrying" ? "Reconnecting…" : "Reconnect"}
        </button>
      }
      footer={
        <>
          {retryState === "ok" && (
            <div className="mt-2 text-xs text-emerald-200/90">
              Spawn requested. The composer will re-enable when the agent is back online.
            </div>
          )}
          {retryState === "failed" && retryError && (
            <div className="mt-2 text-xs text-status-warning/90">Reconnect failed: {retryError}</div>
          )}
        </>
      }
    >
      The agent was terminated via <code className="rounded bg-status-warning/30 px-1">aoe acp stop</code> or an
      equivalent external teardown. New prompts are disabled until you reconnect.
    </StoppedPanel>
  );
}

export function TrashedWorkerStoppedBanner({
  sessionId,
  onRestore,
}: {
  sessionId: string;
  onRestore?: () => Promise<boolean> | void;
}) {
  // On success the banner unmounts, so pending only resets on failure.
  const [restoring, setRestoring] = useState(false);
  const handleRestore = () => {
    if (!onRestore || restoring) return;
    setRestoring(true);
    void Promise.resolve(onRestore()).then(
      (ok) => {
        if (ok === false) setRestoring(false);
      },
      () => setRestoring(false),
    );
  };
  return (
    <StoppedPanel
      testId={`acp-trashed-banner-${sessionId}`}
      title="Session in trash"
      action={
        onRestore && (
          <button type="button" onClick={handleRestore} disabled={restoring} className={WARNING_BUTTON}>
            {restoring ? "Restoring…" : "Restore"}
          </button>
        )
      }
    >
      This session is in the trash. Its transcript and workspace are kept and shown here read-only, but the worker is
      stopped and will not respawn. Restore it to resume, or delete it permanently from the Trash section in the
      sidebar.
    </StoppedPanel>
  );
}

export function ArchivedWorkerStoppedBanner({ sessionId }: { sessionId: string }) {
  return (
    <StoppedPanel testId={`acp-archived-banner-${sessionId}`} title="Session archived">
      This session is parked. The structured view worker was shut down and the reconciler will not respawn it. Unarchive
      from the sidebar (right-click the row, then Unarchive) to bring it back.
    </StoppedPanel>
  );
}

export function SnoozedWorkerStoppedBanner({ sessionId, snoozedUntil }: { sessionId: string; snoozedUntil: string }) {
  const target = new Date(snoozedUntil);
  const wallClock = Number.isFinite(target.getTime()) ? target.toLocaleString() : snoozedUntil;
  return (
    <StoppedPanel testId={`acp-snoozed-banner-${sessionId}`} title="Session snoozed">
      The structured view worker was shut down until <span className="font-mono">{wallClock}</span>. The reconciler will
      respawn it automatically once the snooze expires, or you can Unsnooze from the sidebar (right-click the row) to
      wake it sooner.
    </StoppedPanel>
  );
}

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

import { useRespawnSession } from "../../hooks/useRespawnSession";
import { nativeInputAvailable, type AcpConnectionUnavailability, type AcpState } from "../../lib/acpTypes";
import type { RecoveryActions, RuntimeEvidence, SessionRecovery } from "../../lib/acpTypes";
import { recoverSession, stopSession } from "../../lib/api";
import { Dialog, CancelButton, ConfirmButton, DANGER_BUTTON } from "../Dialog";
import { useConfirmKeys, useDialogFocus } from "../dialogHooks";
import { RecoveryActionRow, type RecoveryAction } from "./RecoveryActionRow";
import { RATTLES } from "../../lib/rattles";
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
  onSwitchToTerminal,
  dismissError,
  startingAgent,
  recoveryCurrent = false,
  readOnly = false,
  onAttach,
  onRetryOperation,
  onRestarted,
}: {
  sessionId: string;
  state: AcpState;
  /** Set while a new Session starts: the Agent, as copy names it. */
  startingAgent: string | null;
  acpWorkerState: "absent" | "resuming" | "running" | "stopping";
  trashedAt: string | null;
  archivedAt: string | null;
  snoozedUntil: string | null;
  onRestore?: () => Promise<boolean> | void;
  onSwitchToTerminal?: () => void;
  dismissError: () => void;
  recoveryCurrent?: boolean;
  readOnly?: boolean;
  onAttach?: () => void;
  onRetryOperation?: () => void;
  onRestarted?: () => void;
}) {
  const nativeReady = nativeInputAvailable(state);
  const startupDiagnosticOnly = nativeReady || state.connectionUnavailable !== null;
  const variant = pickWorkerStoppedVariant({
    workerStopped: state.workerStopped,
    startupError: state.startupError,
    trashedAt,
    archivedAt,
    snoozedUntil,
    workerStopping: acpWorkerState === "stopping",
  });
  const healthy =
    !state.workerStopped &&
    !state.workerRestarting &&
    !state.connectionUnavailable &&
    (!state.startupError || nativeReady);
  const idle = healthy && !state.turnActive;
  const recovery = state.recovery ?? state.recoveryNotice;
  const errorMessage = state.interactionError?.message ?? (recovery ? null : state.lastError);
  return (
    <>
      {state.startupError && !startupDiagnosticOnly && !recovery && (
        <StartupErrorBanner
          sessionId={sessionId}
          message={state.startupError}
          onSwitchToTerminal={onSwitchToTerminal}
        />
      )}
      {state.startupError && startupDiagnosticOnly && (
        <InteractionErrorBanner message={state.startupError} onDismiss={dismissError} />
      )}
      {state.connectionUnavailable ? (
        <NativeChannelNotice observation={state.connectionUnavailable} current />
      ) : state.connectionUnavailableNotice ? (
        <NativeChannelNotice observation={state.connectionUnavailableNotice} current={false} />
      ) : null}
      {state.nativeSnapshotSeq !== null &&
        !nativeReady &&
        !state.connectionUnavailable &&
        !startingAgent &&
        acpWorkerState === "running" && (
          <div
            className="border-b border-surface-700 px-4 py-2 text-xs text-text-muted"
            data-testid="native-channel-unknown"
          >
            Native input readiness is unconfirmed. The transcript remains available.
          </div>
        )}
      {variant === "trashed" && <TrashedWorkerStoppedBanner sessionId={sessionId} onRestore={onRestore} />}
      {variant === "archived" && <ArchivedWorkerStoppedBanner sessionId={sessionId} />}
      {variant === "snoozed" && snoozedUntil && (
        <SnoozedWorkerStoppedBanner sessionId={sessionId} snoozedUntil={snoozedUntil} />
      )}
      {variant === "generic" && !recovery && !state.lastError && <WorkerStoppedBanner sessionId={sessionId} />}
      {state.workerRestarting && !state.startupError && !state.workerStopped && !recovery && (
        <WorkerRestartingBanner agentUnresponsive={state.agentUnresponsive} agentOrphaned={state.agentOrphaned} />
      )}
      {startingAgent ? (
        <StartupWaitLine agent={startingAgent} />
      ) : (
        acpWorkerState === "resuming" &&
        healthy &&
        state.lastSeq > 0 && (
          <PulseBanner>
            Resuming structured view worker… cached transcript still available. Queued prompts will send once the agent
            is back online.
          </PulseBanner>
        )
      )}
      {showWorkerStoppingBanner({ acpWorkerState, startupError: state.startupError }) && <WorkerStoppingBanner />}
      {state.nextWakeupAt && idle && (
        <ScheduledWakeupBanner wakeAt={state.nextWakeupAt} reason={state.nextWakeupReason} />
      )}
      {state.monitorArmed && !state.nextWakeupAt && idle && (
        <ChipBanner tone="violet" icon="👁" detail={state.monitorDescription}>
          Monitoring a background job
        </ChipBanner>
      )}
      {recovery && (
        <SessionRecoveryBanner
          key={sessionId}
          sessionId={sessionId}
          recovery={recovery}
          current={recoveryCurrent && state.recovery !== null}
          readOnly={readOnly}
          requestBusy={state.inflightPromptIds.length > 0}
          onAttach={onAttach}
          onRetryOperation={onRetryOperation}
          onRestarted={onRestarted}
        />
      )}
      {errorMessage && <InteractionErrorBanner message={errorMessage} onDismiss={dismissError} />}
    </>
  );
}

function NativeChannelNotice({ observation, current }: { observation: AcpConnectionUnavailability; current: boolean }) {
  return (
    <div
      className="border-b border-status-warning/30 bg-status-warning/10 px-4 py-2 text-xs text-status-warning"
      data-testid={current ? "native-channel-unavailable" : "native-channel-diagnostic"}
    >
      <div className="font-medium">{current ? "Native input unavailable" : "Previous native-channel diagnostic"}</div>
      <div className="mt-0.5">
        {observation.cause.replaceAll("_", " ")} ({observation.source}; {observation.phase}).
        {current &&
          " The retained conversation has not been replaced. Channel loss alone does not prove the process exited."}
      </div>
    </div>
  );
}

/** From this many seconds the line stops saying "You can type now" and only says it's still starting. */
const STILL_STARTING_AFTER_S = 15;

const reducedMotion = () => window.matchMedia?.("(prefers-reduced-motion: reduce)").matches === true;

/** The one line shown while a new Session starts: the Agent, an elapsed count and
 *  Starting's breathe glyph. Mounting starts the count; unmounting resets it. */
export function StartupWaitLine({ agent }: { agent: string }) {
  const [seconds, setSeconds] = useState(0);
  const [frame, setFrame] = useState(0);
  const [still] = useState(reducedMotion);
  useEffect(() => {
    const started = Date.now();
    const tick = setInterval(() => setSeconds(Math.floor((Date.now() - started) / 1000)), 1000);
    return () => clearInterval(tick);
  }, []);
  useEffect(() => {
    if (still) return;
    const spin = setInterval(() => setFrame((f) => (f + 1) % RATTLES.breathe.frames.length), RATTLES.breathe.interval);
    return () => clearInterval(spin);
  }, [still]);
  const glyph = still ? "⣿" : RATTLES.breathe.frames[frame];
  const elapsed = `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
  return (
    <div
      data-testid="startup-wait-line"
      // Right padding below md clears the header collapse toggle that overhangs this edge.
      className="flex flex-wrap items-center gap-x-2 gap-y-0.5 border-b border-surface-700/60 bg-surface-850 py-2 pl-4 pr-12 text-xs text-text-muted md:pr-4"
    >
      <span aria-hidden className="w-3 shrink-0 font-mono text-text-secondary">
        {glyph}
      </span>
      <span className="min-w-0 flex-1 wrap-anywhere">
        {seconds < STILL_STARTING_AFTER_S ? `Starting ${agent}… You can type now.` : `Still starting ${agent}…`}
      </span>
      <span data-testid="startup-wait-elapsed" className="ml-auto shrink-0 font-mono tabular-nums">
        {elapsed}
      </span>
    </div>
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

function InteractionErrorBanner({
  message,
  onDismiss,
  children,
}: {
  message: string;
  onDismiss?: () => void;
  children?: React.ReactNode;
}) {
  return (
    <div className="flex items-start justify-between gap-3 border-b border-status-warning/30 bg-status-warning/10 px-4 py-2 text-status-warning">
      <div className="flex-1 min-w-0">
        <div className="text-xs font-medium">Action did not complete</div>
        <div className="mt-0.5 text-xs text-status-warning/90 break-words">{message}</div>
        {children}
      </div>
      {onDismiss && (
        <button
          type="button"
          onClick={onDismiss}
          className="shrink-0 rounded-md border border-status-warning/40 bg-status-warning/20 px-2 py-1 text-[10px] font-mono uppercase tracking-wide text-status-warning hover:bg-status-warning/30"
        >
          Dismiss
        </button>
      )}
    </div>
  );
}

function runtimeObservation(runtime: RuntimeEvidence): string {
  if (typeof runtime === "object") {
    const code = runtime.pane_exited.exit_code;
    return `Terminal process exit observed${code === null ? "" : ` (code ${code})`}.`;
  }
  switch (runtime) {
    case "pane_alive":
      return "Terminal pane observed alive.";
    case "acp_transport_ready":
      return "Native channel ready. Channel readiness does not prove the OS process is alive.";
    case "acp_transport_closed":
      return "Native channel closed. Channel loss does not prove the OS process exited.";
    case "unknown":
      return "Runtime liveness is unknown.";
  }
}

/** Render server diagnostics and admission without deriving another lifecycle policy. */
export function SessionRecoveryBanner({
  sessionId,
  recovery,
  current,
  readOnly,
  onAttach,
  requestBusy = false,
  onRetryOperation,
  onRestarted,
}: {
  sessionId: string;
  recovery: SessionRecovery;
  current: boolean;
  readOnly: boolean;
  requestBusy?: boolean;
  onAttach?: () => void;
  onRetryOperation?: () => void;
  onRestarted?: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);
  const [requestError, setRequestError] = useState<string | null>(null);
  const [freshConsent, setFreshConsent] = useState<string | null>(null);
  if (freshConsent !== null && (!current || readOnly || freshConsent !== recovery.expected)) setFreshConsent(null);
  const admission = useRef({
    sessionId,
    recovery,
    current,
    readOnly,
    requestBusy,
    onAttach,
    onRetryOperation,
    onRestarted,
  });
  useLayoutEffect(() => {
    admission.current = {
      sessionId,
      recovery,
      current,
      readOnly,
      requestBusy,
      onAttach,
      onRetryOperation,
      onRestarted,
    };
  });
  useLayoutEffect(
    () => () => {
      admission.current.current = false;
    },
    [],
  );
  const run = useCallback(
    async (action: keyof RecoveryActions) => {
      const latest = admission.current;
      if (
        pending.current ||
        latest.requestBusy ||
        !latest.current ||
        latest.sessionId !== sessionId ||
        latest.recovery.expected !== recovery.expected ||
        !latest.recovery.actions[action] ||
        (action !== "attach" && latest.readOnly)
      )
        return;
      if (action === "attach") return latest.onAttach?.();
      if (action === "retry_operation") return latest.onRetryOperation?.();
      pending.current = true;
      setBusy(true);
      setRequestError(null);
      setFreshConsent(null);
      try {
        const result: { ok: boolean; message?: string } =
          action === "stop"
            ? { ok: (await stopSession(sessionId)) !== null }
            : await recoverSession(sessionId, action === "fresh" ? "fresh" : "same", recovery.expected);
        if (admission.current.sessionId === sessionId) {
          setRequestError(
            result.ok
              ? null
              : (result.message ?? "Recovery request did not complete. Refresh the current observation."),
          );
          if (result.ok && action !== "stop") admission.current.onRestarted?.();
        }
      } finally {
        pending.current = false;
        setBusy(false);
      }
    },
    [sessionId, recovery.expected],
  );
  const handleAction = useCallback(
    (key: keyof RecoveryActions) => {
      if (key === "fresh") setFreshConsent(recovery.expected);
      else void run(key);
    },
    [recovery.expected, run],
  );
  const action = (key: keyof RecoveryActions, label: string, available = true): RecoveryAction => ({
    label,
    testId: `session-recovery-${key.replaceAll("_", "-")}`,
    disabled: !current || busy || requestBusy || !recovery.actions[key] || !available || (key !== "attach" && readOnly),
    onClick: () => handleAction(key),
    describedBy: `session-recovery-guidance-${sessionId}`,
  });
  return (
    <section data-testid="session-recovery" data-reason={recovery.reason} data-session-id={sessionId}>
      <InteractionErrorBanner message={recovery.diagnostic}>
        <div className="mt-1 space-y-1 text-xs text-text-secondary">
          <p data-testid="session-recovery-reason">Reason: {recovery.reason.replaceAll("_", " ")}</p>
          <p data-testid="session-recovery-scope">
            Operation: {recovery.evidence?.operation.replaceAll("_", " ") ?? "unknown"}. Blocking scope:{" "}
            {recovery.evidence?.blocking_scope ?? "unknown"}.
          </p>
          <p data-testid="session-recovery-runtime">
            {current ? runtimeObservation(recovery.runtime) : "Current runtime observation is unavailable."}
          </p>
          <p data-testid="session-recovery-work">
            {!current
              ? "Current Agent work is unknown."
              : recovery.work_in_progress
                ? "Agent work or retry is ongoing."
                : "No current Agent work is reported."}
          </p>
          {recovery.uncertainty && <p data-testid="session-recovery-uncertainty">{recovery.uncertainty}</p>}
          {!current && (
            <p data-testid="session-recovery-stale">
              Previous diagnostic. Current recovery permissions are unavailable; wait for a current server observation.
            </p>
          )}
          <p id={`session-recovery-guidance-${sessionId}`} data-testid="session-recovery-guidance">
            {recovery.guidance}
          </p>
        </div>
        <RecoveryActionRow
          primary={action("attach", "Attach / observe", !!onAttach)}
          secondary={[
            action("retry_operation", "Compose retry", !!onRetryOperation),
            action("restart_same", "Restart Same"),
            action("stop", "Stop"),
            action("fresh", "Fresh / launch without restore flags"),
          ]}
          install={{ error: null, output: null }}
        />
        {requestError && (
          <p role="alert" className="mt-2 text-xs">
            {requestError}
          </p>
        )}
      </InteractionErrorBanner>
      {freshConsent === recovery.expected && current && recovery.actions.fresh && !readOnly && (
        <FreshRecoveryConfirm onCancel={() => setFreshConsent(null)} onConfirm={() => void run("fresh")} />
      )}
    </section>
  );
}

function FreshRecoveryConfirm({ onCancel, onConfirm }: { onCancel: () => void; onConfirm: () => void }) {
  const confirmRef = useRef<HTMLButtonElement | null>(null);
  useDialogFocus(confirmRef);
  useConfirmKeys(onCancel, onConfirm);
  return (
    <Dialog
      id="session-recovery-fresh-dialog"
      title="Launch without restore flags?"
      onDismiss={onCancel}
      footer={
        <>
          <CancelButton onClick={onCancel} />
          <ConfirmButton
            buttonRef={confirmRef}
            onClick={onConfirm}
            testId="session-recovery-fresh-confirm"
            className={DANGER_BUTTON}
          >
            Launch without restore flags
          </ConfirmButton>
        </>
      }
    >
      <p className="text-[13px] text-text-secondary">
        This stops the current runtime and launches without AoE restore flags. Existing native history is not deleted.
        An undeclared wrapper may still restore its own history; this does not promise a new native conversation.
        Pending AoE input is discarded. Blocked or possibly accepted prompts will not be replayed.
      </p>
    </Dialog>
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

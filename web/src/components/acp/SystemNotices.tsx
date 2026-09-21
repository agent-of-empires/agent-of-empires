import { useState } from "react";

import type { RespawnState } from "../../hooks/useRespawnSession";
import type { AcpState } from "../../lib/acpTypes";
import type { AcpContext } from "./AcpRuntime";
import { SwitchAgentModal } from "./SwitchAgentModal";

/** Owns the rate-limit recovery modal toggle and hands its opener to `children`. */
export function RateLimitRecoverySection({
  sessionId,
  currentAgent,
  onPrefill,
  children,
}: {
  sessionId: string;
  currentAgent: string | null;
  onPrefill: (text: string) => void;
  children: (renderProps: { onSwitchAgent: () => void }) => React.ReactNode;
}) {
  const [open, setOpen] = useState(false);
  return (
    <>
      {children({ onSwitchAgent: () => setOpen(true) })}
      <SwitchAgentModal
        open={open}
        sessionId={sessionId}
        currentAgent={currentAgent}
        onClose={() => setOpen(false)}
        onPrefill={onPrefill}
        trigger="rate_limit"
      />
    </>
  );
}

/** The agent's rate-limit wording without transport prefixes or the trailing
 *  `{"errorKind":...}` fingerprint the connection-end path appends. */
function rateLimitWording(status: string): string {
  const text = status
    .replace(/[\s:]*\{[\s\S]*\}\s*$/, "")
    .replace(/^(?:ACP connection failed:\s*)?(?:Internal error:?\s*)?/, "")
    .trim();
  return text || "the agent did not report a reset time.";
}

const ACTION_BUTTON =
  "shrink-0 rounded-md border border-brand-700 bg-brand-900/40 px-2 py-1 text-[10px] font-mono uppercase tracking-wide text-brand-100 hover:bg-brand-900/60";

export function SystemNotices({
  status,
  lagged,
  rateLimit,
  rateLimitAutoResume,
  rateLimitRetriesExhausted,
  hasEverOpened,
  reconnecting,
  retryCount,
  retryCountdown,
  maxRetries,
  manualReconnect,
  onSwitchAgent,
  onResumeRateLimit,
  rateLimitResumeState = "idle",
  rateLimitResumeError = null,
}: {
  status: AcpContext["status"];
  lagged: boolean;
  rateLimit: AcpState["rateLimit"];
  /** Omitted when unknown, in which case nothing is claimed about auto-resume. */
  rateLimitAutoResume?: boolean;
  rateLimitRetriesExhausted: boolean;
  hasEverOpened: boolean;
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  maxRetries: number;
  manualReconnect: () => void;
  onSwitchAgent?: () => void;
  onResumeRateLimit?: () => void;
  rateLimitResumeState?: RespawnState;
  rateLimitResumeError?: string | null;
}) {
  const messages: { kind: "warn" | "info" | "muted"; text: string }[] = [];
  const warn = (text: string) => messages.push({ kind: "warn", text });
  const reconnectRetriesExhausted = status !== "open" && hasEverOpened && !reconnecting && retryCount >= maxRetries;
  if (reconnecting && status !== "open") {
    const countdownPart = retryCountdown > 0 ? ` in ${retryCountdown}s` : "";
    warn(`Structured view disconnected. Reconnecting (${retryCount}/${maxRetries})${countdownPart}…`);
  } else if (status === "connecting") {
    messages.push({
      kind: "info",
      text: hasEverOpened ? "Reconnecting to structured view…" : "Starting structured view…",
    });
  } else if (status === "error") {
    warn(
      hasEverOpened
        ? "Structured view reconnecting… showing cached transcript; new messages disabled."
        : "Starting structured view worker… this can take a few seconds for new sessions.",
    );
  } else if (status === "closed" && !reconnectRetriesExhausted) {
    warn(
      hasEverOpened
        ? "Structured view disconnected. Showing cached transcript; new messages disabled."
        : "Structured view not ready yet. Retrying…",
    );
  }
  if (lagged) warn("Some events were missed during reconnect.");
  if (rateLimit) {
    // Without a parseable reset, show the agent's own wording rather than a made-up time.
    const reset = rateLimit.resets_at === null ? null : new Date(rateLimit.resets_at);
    warn(
      reset && !Number.isNaN(reset.getTime())
        ? `Rate-limited (${rateLimit.kind}); resets at ${reset.toLocaleTimeString()}.`
        : `Rate-limited (${rateLimit.kind}); ${rateLimitWording(rateLimit.status)}`,
    );
    if (rateLimitAutoResume === true && !rateLimitRetriesExhausted) {
      messages.push({ kind: "muted", text: "Auto-resume is armed; the session resumes when the window clears." });
    } else if (rateLimitAutoResume === false) {
      messages.push({
        kind: "muted",
        text: "Auto-resume is off for this profile; use Resume now, or enable acp.rate_limit_auto_resume.",
      });
    }
  }
  if (rateLimitRetriesExhausted) {
    warn(
      "Auto-resume stopped: the same prompt was re-sent too many times without getting through. Resume manually or send a new prompt.",
    );
  }
  const resumePending = rateLimitResumeState === "retrying" || rateLimitResumeState === "ok";
  if (messages.length === 0 && !reconnectRetriesExhausted) return null;
  return (
    <div className="border-b border-surface-800 px-4 py-2 space-y-1">
      {messages.map((m, i) => (
        <div key={i} className={`text-xs ${m.kind === "warn" ? "text-brand-400" : "text-text-muted"}`}>
          {m.text}
        </div>
      ))}
      {rateLimit && (onResumeRateLimit || onSwitchAgent) && (
        <div className="flex flex-wrap items-center justify-end gap-2 pt-1">
          {onResumeRateLimit && (
            <button
              type="button"
              onClick={onResumeRateLimit}
              disabled={resumePending}
              className={`${ACTION_BUTTON} disabled:cursor-not-allowed disabled:opacity-60`}
            >
              {rateLimitResumeState === "retrying"
                ? "Resuming…"
                : rateLimitResumeState === "ok"
                  ? "Resume requested"
                  : "Resume now"}
            </button>
          )}
          {onSwitchAgent && (
            <button type="button" onClick={onSwitchAgent} className={ACTION_BUTTON}>
              Continue in another agent
            </button>
          )}
        </div>
      )}
      {rateLimit && rateLimitResumeState === "ok" && (
        <div className="pt-1 text-xs text-text-muted">Resume requested. New events should start streaming shortly.</div>
      )}
      {rateLimit && rateLimitResumeState === "failed" && rateLimitResumeError && (
        <div className="pt-1 text-xs text-brand-400">Resume failed: {rateLimitResumeError}</div>
      )}
      {reconnectRetriesExhausted && (
        <div className="flex items-center justify-between gap-3 text-xs text-brand-400">
          <span>Connection lost. Auto-retry stopped.</span>
          <button type="button" onClick={manualReconnect} className={ACTION_BUTTON}>
            Reconnect
          </button>
        </div>
      )}
    </div>
  );
}

import type { AcpContext } from "./AcpRuntime";
import { connectionRetriesExhausted } from "./startupWait";

const ACTION_BUTTON =
  "shrink-0 rounded-md border border-brand-700 bg-brand-900/40 px-2 py-1 text-[10px] font-mono uppercase tracking-wide text-brand-100 hover:bg-brand-900/60";

export function SystemNotices({
  status,
  lagged,
  rateLimitRetriesExhausted,
  hasEverOpened,
  replayedWhileDown = false,
  reconnecting,
  retryCount,
  retryCountdown,
  maxRetries,
  manualReconnect,
  newSessionStarting = false,
}: {
  status: AcpContext["status"];
  lagged: boolean;
  /** Only while no usage-limit banner is shown; the banner says it otherwise. */
  rateLimitRetriesExhausted: boolean;
  hasEverOpened: boolean;
  /** Events still arrive over HTTP replay while the socket is down. */
  replayedWhileDown?: boolean;
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  maxRetries: number;
  manualReconnect: () => void;
  /** The startup wait line already covers a new Session's first connection. */
  newSessionStarting?: boolean;
}) {
  const messages: { kind: "warn" | "info"; text: string }[] = [];
  const warn = (text: string) => messages.push({ kind: "warn", text });
  const reconnectRetriesExhausted = connectionRetriesExhausted({ status, reconnecting, retryCount, maxRetries });
  if (!reconnectRetriesExhausted && !(newSessionStarting && !hasEverOpened)) {
    if (reconnecting && status !== "open") {
      const countdownPart = retryCountdown > 0 ? ` in ${retryCountdown}s` : "";
      const lead = replayedWhileDown
        ? "Live updates unavailable, showing history fetched over HTTP."
        : "Structured view disconnected.";
      warn(`${lead} Reconnecting (${retryCount}/${maxRetries})${countdownPart}…`);
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
    } else if (status === "closed") {
      warn(
        hasEverOpened
          ? "Structured view disconnected. Showing cached transcript; new messages disabled."
          : "Structured view not ready yet. Retrying…",
      );
    }
  }
  if (lagged) warn("Some events were missed during reconnect.");
  if (rateLimitRetriesExhausted) {
    warn("Auto-resume stopped: the same prompt was re-sent too many times without getting through. Send a new prompt.");
  }
  if (messages.length === 0 && !reconnectRetriesExhausted) return null;
  return (
    <div className="border-b border-surface-800 px-4 py-2 space-y-1">
      {messages.map((m, i) => (
        <div key={i} className={`text-xs ${m.kind === "warn" ? "text-brand-400" : "text-text-muted"}`}>
          {m.text}
        </div>
      ))}
      {reconnectRetriesExhausted && (
        <div className="flex items-center justify-between gap-3 text-xs text-brand-400">
          <span>
            {hasEverOpened ? "Connection lost. Auto-retry stopped." : "Could not connect. Auto-retry stopped."}
          </span>
          <button type="button" onClick={manualReconnect} className={ACTION_BUTTON}>
            Reconnect
          </button>
        </div>
      )}
    </div>
  );
}

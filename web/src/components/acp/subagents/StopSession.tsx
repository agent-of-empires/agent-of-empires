// ACP has no per-agent cancel, so Stop interrupts the whole session; confirm before sending it.

import { Square } from "lucide-react";
import { useState } from "react";

import { reportError } from "../../../lib/toastBus";

export function StopSession({ sessionId, running }: { sessionId: string; running: number }) {
  // Holds the session the confirmation was opened for, so switching sessions closes it.
  const [confirmingFor, setConfirmingFor] = useState<string | null>(null);
  const confirming = confirmingFor === sessionId;
  const [busy, setBusy] = useState(false);
  const stop = async () => {
    setBusy(true);
    try {
      const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/cancel`, { method: "POST" });
      if (!res.ok) reportError("Could not interrupt the session. Try again.");
    } catch {
      reportError("Could not interrupt the session. Try again.");
    } finally {
      setBusy(false);
      setConfirmingFor(null);
    }
  };
  return (
    <>
      <button
        type="button"
        title="Interrupt the session and stop running sub-agents"
        onClick={() => setConfirmingFor(sessionId)}
        className={[
          "inline-flex items-center gap-1 rounded-md border border-surface-600 bg-surface-800 px-2 py-0.5",
          "text-[11px] text-text-secondary transition-colors",
          "hover:border-rose-700/60 hover:bg-rose-950/30 hover:text-rose-300",
        ].join(" ")}
      >
        <Square className="h-3 w-3 fill-current" strokeWidth={0} />
        Stop
      </button>
      {confirming && (
        <div
          role="alertdialog"
          aria-label="Confirm stop"
          className="basis-full rounded-md border border-rose-700/50 bg-rose-950/20 px-3 py-2 text-xs text-text-secondary"
        >
          <p>
            This interrupts the main agent and all {running > 0 ? `${running} ` : ""}running sub-agent
            {running === 1 ? "" : "s"}. Work already written to disk stays.
          </p>
          <div className="mt-2 flex gap-2">
            <button
              type="button"
              disabled={busy}
              onClick={() => void stop()}
              className="rounded border border-rose-700/60 px-2 py-0.5 text-rose-300 hover:bg-rose-950/40 disabled:opacity-50"
            >
              Interrupt session
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => setConfirmingFor(null)}
              className="px-2 py-0.5 hover:text-text-bright disabled:opacity-50"
            >
              Cancel
            </button>
          </div>
        </div>
      )}
    </>
  );
}

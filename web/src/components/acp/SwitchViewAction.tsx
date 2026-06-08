// Small button + destructive-confirm dialog for switching a session
// between structured view and terminal view. Used from both the structured-view
// composer (offers "Switch to terminal view") and the terminal view
// (offers "Switch to structured view"). After the API call returns, the
// session-list poll picks up the updated `structured_view` and the
// parent flips between <StructuredView> and <TerminalView>.

import { useEffect, useRef, useState } from "react";
import { ArrowRightLeft, Loader2 } from "lucide-react";
import { useServerDown, OFFLINE_TITLE } from "../../lib/connectionState";

interface Props {
  sessionId: string;
  /** Current view. Determines which direction the swap goes. */
  structuredView: boolean;
  /** ACP-capable: when false (e.g. tool=aider), the switch-to-structured-view
   *  button is disabled. The terminal-view side passes the wizard's
   *  ACP_CAPABLE_TOOLS check; the structured-view side never sees this
   *  prop because the structured view can always go back to the terminal. */
  acpCapable?: boolean;
  /** Optional className override on the trigger button. */
  className?: string;
  /** Render style: an icon button (compact, for toolbars) or a full
   *  button with text (for banner contexts). */
  variant?: "icon" | "button";
}

export function SwitchViewAction({
  sessionId,
  structuredView,
  acpCapable = true,
  className,
  variant = "icon",
}: Props) {
  const offline = useServerDown();
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);

  // Close on outside click / Esc.
  useEffect(() => {
    if (!confirmOpen) return;
    const onClick = (e: MouseEvent) => {
      if (!dialogRef.current?.contains(e.target as Node)) setConfirmOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setConfirmOpen(false);
    };
    document.addEventListener("mousedown", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [confirmOpen]);

  const target = structuredView ? "terminal view" : "structured view";
  const endpoint = structuredView
    ? `/api/sessions/${encodeURIComponent(sessionId)}/acp/disable`
    : `/api/sessions/${encodeURIComponent(sessionId)}/acp/enable`;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const res = await fetch(endpoint, { method: "POST" });
      if (!res.ok) {
        const text = await res.text();
        setError(text || `HTTP ${res.status}`);
        setBusy(false);
        return;
      }
      // Session list polls every 3s; the parent will swap views the
      // next tick. Close the dialog optimistically.
      setConfirmOpen(false);
      setBusy(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  const triggerLabel = structuredView
    ? "Switch to terminal view"
    : "Switch to structured view";
  const triggerDisabled = (!structuredView && !acpCapable) || offline;

  return (
    <>
      <button
        type="button"
        title={
          offline
            ? OFFLINE_TITLE
            : triggerDisabled
              ? "This agent has no ACP adapter, structured view unavailable"
              : triggerLabel
        }
        aria-label={triggerLabel}
        disabled={triggerDisabled}
        onClick={() => setConfirmOpen(true)}
        className={
          className ??
          [
            variant === "button"
              ? "inline-flex items-center gap-1.5 rounded-md border border-surface-700 bg-surface-800 px-2.5 py-1.5 text-[12px] font-medium text-text-secondary hover:bg-surface-700"
              : "inline-flex h-7 w-7 items-center justify-center rounded-md text-text-dim hover:bg-surface-800 hover:text-text-secondary",
            "transition-colors disabled:cursor-not-allowed disabled:opacity-50",
          ].join(" ")
        }
      >
        <ArrowRightLeft className="h-3.5 w-3.5" />
        {variant === "button" && <span>Switch to {target}</span>}
      </button>

      {confirmOpen && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40"
          role="dialog"
          aria-modal="true"
        >
          <div
            ref={dialogRef}
            className="w-[26rem] max-w-[92vw] rounded-xl border border-surface-700 bg-surface-900 p-4 shadow-xl"
          >
            <h2 className="text-sm font-semibold text-text-primary">
              Switch to {target}?
            </h2>
            <p className="mt-2 text-xs leading-relaxed text-text-muted">
              {structuredView ? (
                <>
                  The structured view conversation history will be discarded and
                  the agent will restart in a fresh terminal pane. Open files
                  and worktree state are preserved.
                </>
              ) : (
                <>
                  The current tmux scrollback will be lost and the agent will
                  restart as an ACP server. Open files and worktree state are
                  preserved.
                </>
              )}
            </p>
            {error && (
              <p className="mt-2 rounded bg-rose-950/40 px-2 py-1 text-xs text-rose-300">
                {error}
              </p>
            )}
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setConfirmOpen(false)}
                disabled={busy}
                className="rounded-md border border-surface-700 bg-surface-800 px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-surface-700 disabled:cursor-not-allowed disabled:opacity-60"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => void submit()}
                disabled={busy}
                className="inline-flex items-center gap-1.5 rounded-md bg-brand-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-brand-500 disabled:cursor-not-allowed disabled:opacity-70"
              >
                {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                Switch
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}

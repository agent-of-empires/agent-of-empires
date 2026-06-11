import { useCallback, useEffect, useState } from "react";
import { useTerminal } from "../hooks/useTerminal";
import { useIsCoarsePointer } from "../hooks/useIsCoarsePointer";
import { LiveTerminalView } from "./LiveTerminalView";
import { TerminalConnectionBanners } from "./TerminalConnectionBanners";
import { ensureTerminal } from "../lib/api";
import type { SessionResponse } from "../lib/types";
import {
  FOCUS_TERMINAL_EVENT,
  consumePendingTerminalFocus,
  setPendingTerminalFocus,
  type FocusTerminalDetail,
} from "../lib/terminalFocus";
import "@xterm/xterm/css/xterm.css";

type ShellMode = "host" | "container";

/** The paired (side-shell) xterm.js terminal, desktop only: touch
 *  devices render the capture-snapshot LiveTerminalView instead (see
 *  PairedShellPane), so this component carries no mobile machinery. */
function PairedTerminal({ sessionId, mode }: { sessionId: string; mode: ShellMode }) {
  const [ready, setReady] = useState(false);
  const wsPath = mode === "container" ? "container-terminal/ws" : "terminal/ws";
  const { containerRef, termRef, state, manualReconnect, activate, maxRetries } = useTerminal(
    ready ? sessionId : null,
    wsPath,
    false,
  );
  const [termFocused, setTermFocused] = useState(false);
  const [bootError, setBootError] = useState(false);
  const [bootAttempt, setBootAttempt] = useState(0);

  const focusSelf = useCallback(() => {
    const ta = termRef.current?.element?.querySelector("textarea");
    if (ta instanceof HTMLElement) {
      ta.focus();
      return true;
    }
    return false;
  }, [termRef]);

  // Track effect key changes to reset ready/bootError during render
  const effectKey = `${sessionId}-${mode}-${bootAttempt}`;
  const [trackedEffectKey, setTrackedEffectKey] = useState(effectKey);
  if (effectKey !== trackedEffectKey) {
    setTrackedEffectKey(effectKey);
    setReady(false);
    setBootError(false);
  }

  useEffect(() => {
    let cancelled = false;
    void ensureTerminal(sessionId, mode === "container")
      .then((ok) => {
        if (cancelled) return;
        if (ok) {
          setReady(true);
        } else setBootError(true);
      })
      .catch(() => {
        if (!cancelled) setBootError(true);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, mode, bootAttempt, focusSelf]);

  // Drain a pending paired-focus latch only after `ready` flips and the
  // terminal renders: while !ready the splash is shown and the xterm textarea
  // is not mounted, so consuming the latch in the ensureTerminal callback
  // would clear it before focusSelf() could find anything to focus.
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (!ready) return;
    if (consumePendingTerminalFocus("paired")) focusSelf();
  }, [ready, focusSelf]);

  // Returns true if focus was applied. Callers can fall back to the pending
  // latch when the textarea isn't in the DOM yet (PTY still booting).
  // Cmd+` shortcut focuses this terminal when "paired" is the dispatched
  // target. The component might be mounted but its PTY not yet ready (the
  // initial ensureTerminal round-trip), in which case focusSelf() can't
  // find a textarea, so we latch the intent; the ensureTerminal callback
  // drains the latch once the PTY boots. While the right panel is
  // collapsed this component is unmounted entirely; App.tsx sets the latch
  // directly in that case.
  useEffect(() => {
    const onFocusEvent = (e: Event) => {
      const detail = (e as CustomEvent<FocusTerminalDetail>).detail;
      if (detail?.target !== "paired") return;
      if (!focusSelf()) setPendingTerminalFocus("paired");
    };
    window.addEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
    return () => window.removeEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
  }, [focusSelf]);

  if (bootError) {
    return (
      <div className="flex-1 flex flex-col items-center justify-center gap-2 bg-surface-950 text-text-dim">
        <span className="text-xs text-status-error">Couldn't start the terminal.</span>
        <button
          onClick={() => setBootAttempt((n) => n + 1)}
          className="text-xs text-brand-500 cursor-pointer underline"
        >
          Retry
        </button>
      </div>
    );
  }

  if (!ready) {
    return (
      <div className="flex-1 flex items-center justify-center bg-surface-950 text-text-dim">
        <span className="text-xs">Starting terminal...</span>
      </div>
    );
  }

  return (
    <div className="flex-1 flex flex-col min-h-0 overflow-hidden">
      <TerminalConnectionBanners
        connected={state.connected}
        reconnecting={state.reconnecting}
        retryCount={state.retryCount}
        retryCountdown={state.retryCountdown}
        maxRetries={maxRetries}
        onRetry={manualReconnect}
      />
      <div
        data-term="paired"
        className={`flex-1 overflow-hidden bg-[var(--term-bg)] relative term-panel${termFocused ? " term-focused" : ""}`}
        onFocus={() => setTermFocused(true)}
        onBlur={() => setTermFocused(false)}
      >
        <div ref={containerRef} className="absolute inset-0" onPointerDown={activate} />
      </div>
    </div>
  );
}

/** Host/container shell switch plus the paired terminal. Used both in the
 *  desktop right-panel split (`fullViewport={false}`) and as the promoted
 *  single full-viewport mobile pane (`fullViewport`). */
export function PairedShellPane({ session, sessionId }: { session: SessionResponse | null; sessionId: string | null }) {
  const [shellMode, setShellMode] = useState<ShellMode>("host");
  const isSandboxed = session?.is_sandboxed ?? false;
  // Touch devices get the capture-snapshot live view (same architecture
  // as the agent pane); fine pointers keep the xterm PTY relay.
  const coarse = useIsCoarsePointer();

  return (
    <div className="flex-1 flex flex-col min-h-0 overflow-hidden">
      <div className="flex items-center gap-1 px-2 py-1 bg-surface-900 border-b border-surface-700/20 shrink-0">
        <span className="text-xs text-text-dim mr-1">Shell</span>
        <button
          onClick={() => setShellMode("host")}
          className={`text-[12px] px-2 py-0.5 rounded cursor-pointer transition-colors ${
            shellMode === "host" ? "text-brand-500 bg-brand-600/10" : "text-text-dim hover:text-text-muted"
          }`}
        >
          Host
        </button>
        {isSandboxed && (
          <button
            onClick={() => setShellMode("container")}
            className={`text-[12px] px-2 py-0.5 rounded cursor-pointer transition-colors ${
              shellMode === "container" ? "text-brand-500 bg-brand-600/10" : "text-text-dim hover:text-text-muted"
            }`}
          >
            Container
          </button>
        )}
      </div>

      {sessionId ? (
        coarse && session ? (
          <LiveTerminalView
            key={`${sessionId}-${shellMode}`}
            session={session}
            surface={shellMode === "container" ? "paired-container" : "paired-host"}
          />
        ) : (
          <PairedTerminal key={`${sessionId}-${shellMode}`} sessionId={sessionId} mode={shellMode} />
        )
      ) : (
        <div className="flex-1 flex items-center justify-center bg-surface-950 text-text-dim">
          <p className="text-xs">Select a session</p>
        </div>
      )}
    </div>
  );
}

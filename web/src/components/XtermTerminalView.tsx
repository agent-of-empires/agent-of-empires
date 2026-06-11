// Desktop (fine-pointer) agent terminal: the xterm.js + PTY attach relay.
// Touch devices render LiveTerminalView instead (capture-snapshot live
// mode); TerminalView.tsx dispatches between the two.
import { useCallback, useEffect, useRef, useState } from "react";
import { useTerminal } from "../hooks/useTerminal";
import { TerminalConnectionBanners } from "./TerminalConnectionBanners";
import { ensureSession } from "../lib/api";
import type { SessionResponse } from "../lib/types";
import {
  FOCUS_TERMINAL_EVENT,
  consumePendingTerminalFocus,
  setPendingTerminalFocus,
  type FocusTerminalDetail,
} from "../lib/terminalFocus";
import "@xterm/xterm/css/xterm.css";

interface Props {
  session: SessionResponse;
  active?: boolean;
}

export function XtermTerminalView({ session, active = true }: Props) {
  const [ensureState, setEnsureState] = useState<"pending" | "ready" | "error">("pending");
  const [ensureError, setEnsureError] = useState<string | null>(null);
  const { containerRef, termRef, state, manualReconnect, activate, maxRetries } = useTerminal(
    ensureState === "ready" ? session.id : null,
    "ws",
    active,
    session.claude_fullscreen,
    active,
  );
  const [trackedSessionId, setTrackedSessionId] = useState(session.id);
  if (session.id !== trackedSessionId) {
    setTrackedSessionId(session.id);
    setEnsureState("pending");
    setEnsureError(null);
  }
  const lastEnsuredSessionIdRef = useRef<string | null>(null);
  const [termFocused, setTermFocused] = useState(false);

  const focusSelf = useCallback(() => {
    const ta = termRef.current?.element?.querySelector("textarea");
    if (ta instanceof HTMLElement) {
      ta.focus();
      return true;
    }
    return false;
  }, [termRef]);

  useEffect(() => {
    if (lastEnsuredSessionIdRef.current === session.id) {
      if (active) activate();
      if (consumePendingTerminalFocus("agent")) focusSelf();
      return;
    }
    const controller = new AbortController();
    ensureSession(session.id, controller.signal).then((res) => {
      if (controller.signal.aborted) return;
      if (res.ok) {
        lastEnsuredSessionIdRef.current = session.id;
        setEnsureState("ready");
        if (active) activate();
      } else {
        setEnsureState("error");
        setEnsureError(res.message ?? "Could not start session.");
      }
    });
    return () => controller.abort();
  }, [session.id, active, activate, focusSelf]);

  // Drain a pending agent-focus latch only once the terminal is rendered:
  // while ensureState is "pending"/"error" the splash is shown and the xterm
  // textarea is not mounted, so consuming the latch in the boot/retry
  // callbacks would clear it before focusSelf() could find anything to focus.
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (ensureState !== "ready") return;
    if (consumePendingTerminalFocus("agent")) focusSelf();
  }, [ensureState, focusSelf]);

  const retryEnsure = useCallback(() => {
    setEnsureState((prev) => {
      if (prev === "pending") return prev;
      setEnsureError(null);
      const controller = new AbortController();
      ensureSession(session.id, controller.signal).then((res) => {
        if (controller.signal.aborted) return;
        if (res.ok) {
          lastEnsuredSessionIdRef.current = session.id;
          setEnsureState("ready");
          if (active) activate();
        } else {
          setEnsureState("error");
          setEnsureError(res.message ?? "Could not start session.");
        }
      });
      return "pending";
    });
  }, [session.id, active, activate]);

  // Cmd+` shortcut focuses this terminal when "agent" is the dispatched target.
  useEffect(() => {
    const onFocusEvent = (e: Event) => {
      const detail = (e as CustomEvent<FocusTerminalDetail>).detail;
      if (detail?.target !== "agent") return;
      if (!focusSelf()) setPendingTerminalFocus("agent");
    };
    window.addEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
    return () => window.removeEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
  }, [focusSelf]);

  if (ensureState === "pending") {
    return (
      <div className="flex-1 flex items-center justify-center bg-surface-950 text-text-dim">
        <span className="text-xs">Starting session...</span>
      </div>
    );
  }

  if (ensureState === "error") {
    return (
      <div className="flex-1 flex flex-col items-center justify-center bg-surface-950 gap-2 px-4 text-center">
        <span className="text-xs text-status-error max-w-md break-words">
          {ensureError ?? "Could not start session."}
        </span>
        <button onClick={retryEnsure} className="text-xs text-brand-500 hover:text-brand-400 cursor-pointer underline">
          Retry
        </button>
      </div>
    );
  }

  return (
    <div className="flex-1 flex flex-col overflow-hidden relative">
      <TerminalConnectionBanners
        connected={state.connected}
        reconnecting={state.reconnecting}
        retryCount={state.retryCount}
        retryCountdown={state.retryCountdown}
        maxRetries={maxRetries}
        onRetry={manualReconnect}
      />

      <div
        data-term="agent"
        className={`flex-1 overflow-hidden bg-[var(--term-bg)] relative term-panel${termFocused ? " term-focused" : ""}`}
        onFocus={() => setTermFocused(true)}
        onBlur={() => setTermFocused(false)}
      >
        <div ref={containerRef} className="absolute inset-0" onPointerDown={activate} />

        {state.connected && !state.isPrimary && (
          <div
            aria-hidden="true"
            className="absolute left-0 right-0 top-3 flex justify-center pointer-events-none z-10"
          >
            <span className="font-mono text-[11px] text-text-dim bg-surface-800/80 border border-surface-700/50 rounded-md px-2.5 py-1 backdrop-blur-sm">
              Viewing from another device. Click to take over.
            </span>
          </div>
        )}
      </div>
    </div>
  );
}

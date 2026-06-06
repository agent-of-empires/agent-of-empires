import { useCallback, useEffect, useLayoutEffect, useState } from "react";
import { useTerminal } from "../hooks/useTerminal";
import { useMobileKeyboard } from "../hooks/useMobileKeyboard";
import { MobileTerminalToolbar } from "./MobileTerminalToolbar";
import { BackToLiveButton } from "./BackToLiveButton";
import { KeyboardFab } from "./KeyboardFab";
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

/** The paired (side-shell) xterm.js terminal.
 *
 *  `fullViewport` switches the keyboard-padding posture. In the desktop
 *  split this terminal owns only half of an already-capped panel, so it
 *  pads by the live `keyboardHeight` (and on desktop that is always 0).
 *  When promoted to the single full-viewport mobile pane it owns the
 *  whole viewport, so it pads by `keyboardOcclusion`, the same value the
 *  agent `TerminalView` uses; padding by the live height there would
 *  collapse the grid on every keyboard show/hide. See #1452. */
function PairedTerminal({
  sessionId,
  mode,
  fullViewport = false,
}: {
  sessionId: string;
  mode: ShellMode;
  fullViewport?: boolean;
}) {
  const [ready, setReady] = useState(false);
  const wsPath = mode === "container" ? "container-terminal/ws" : "terminal/ws";
  const {
    containerRef,
    termRef,
    state,
    manualReconnect,
    sendData,
    activate,
    exitScrollback,
    ctrlActiveRef,
    clearCtrlRef,
    maxRetries,
  } = useTerminal(ready ? sessionId : null, wsPath, false);
  const { isMobile, keyboardOpen, keyboardHeight, keyboardOcclusion } =
    useMobileKeyboard();
  const [ctrlActive, setCtrlActive] = useState(false);
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

  // See TerminalView.tsx for why these syncs live in effects rather
  // than running during render.
  useEffect(() => {
    ctrlActiveRef.current = ctrlActive;
  });
  useEffect(() => {
    clearCtrlRef.current = () => setCtrlActive(false);
  }, [clearCtrlRef]);

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

  // Dispatch a window resize after keyboard transitions so anything else
  // watching layout is nudged; the hook's ResizeObserver already refits
  // the terminal grid automatically.
  useLayoutEffect(() => {
    const t = setTimeout(() => {
      window.dispatchEvent(new Event("resize"));
    }, 150);
    return () => clearTimeout(t);
  }, [keyboardHeight, keyboardOpen]);

  const toggleKeyboard = useCallback(() => {
    const term = termRef.current;
    if (!term?.element) return;
    const ta = term.element.querySelector("textarea");
    if (keyboardOpen) {
      ta?.blur();
    } else if (ta instanceof HTMLElement) {
      ta.focus();
    }
    activate();
  }, [termRef, keyboardOpen, activate]);

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
        <span className="text-xs text-status-error">
          Couldn't start the terminal.
        </span>
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

  const appliedKeyboardPadding = fullViewport
    ? keyboardOcclusion
    : keyboardHeight;
  const rootStyle = {
    paddingBottom:
      appliedKeyboardPadding > 0 ? appliedKeyboardPadding : undefined,
  } as const;

  return (
    <div
      className="flex-1 flex flex-col min-h-0 overflow-hidden md:bg-surface-800"
      style={rootStyle}
    >
      {!state.connected && state.reconnecting && (
        <div className="bg-status-waiting/15 border-b border-status-waiting/30 px-3 py-1 shrink-0">
          <span className="text-xs text-status-waiting">
            Reconnecting... ({state.retryCount}/{maxRetries})
          </span>
        </div>
      )}
      {!state.connected &&
        !state.reconnecting &&
        state.retryCount >= maxRetries && (
          <div className="bg-status-error/10 border-b border-status-error/30 px-3 py-1 flex items-center gap-2 shrink-0">
            <span className="text-xs text-status-error">Disconnected</span>
            <button
              onClick={manualReconnect}
              className="text-xs text-brand-500 cursor-pointer underline"
            >
              Retry
            </button>
          </div>
        )}
      <div
        data-term="paired"
        className={`flex-1 overflow-hidden bg-surface-950 relative md:rounded-lg term-panel${termFocused ? " term-focused" : ""}`}
        onFocus={() => setTermFocused(true)}
        onBlur={() => setTermFocused(false)}
      >
        <div
          ref={containerRef}
          className="absolute inset-0"
          onPointerDown={activate}
        />

        {isMobile && state.isInScrollback && (
          <BackToLiveButton onClick={exitScrollback} topOffset="top-2" />
        )}

        {isMobile && state.connected && (
          <KeyboardFab keyboardOpen={keyboardOpen} onToggle={toggleKeyboard} />
        )}
      </div>
      {isMobile && state.connected && (
        <MobileTerminalToolbar
          sendData={sendData}
          termRef={termRef}
          keyboardOpen={keyboardOpen}
          ctrlActive={ctrlActive}
          onCtrlToggle={() => setCtrlActive((v) => !v)}
        />
      )}
    </div>
  );
}

/** Host/container shell switch plus the paired terminal. Used both in the
 *  desktop right-panel split (`fullViewport={false}`) and as the promoted
 *  single full-viewport mobile pane (`fullViewport`). */
export function PairedShellPane({
  session,
  sessionId,
  fullViewport = false,
}: {
  session: SessionResponse | null;
  sessionId: string | null;
  fullViewport?: boolean;
}) {
  const [shellMode, setShellMode] = useState<ShellMode>("host");
  const isSandboxed = session?.is_sandboxed ?? false;

  return (
    <div className="flex-1 flex flex-col min-h-0 overflow-hidden">
      <div className="flex items-center gap-1 px-2 py-1 bg-surface-900 border-b border-surface-700/20 shrink-0">
        <span className="text-xs text-text-dim mr-1">Shell</span>
        <button
          onClick={() => setShellMode("host")}
          className={`text-[12px] px-2 py-0.5 rounded cursor-pointer transition-colors ${
            shellMode === "host"
              ? "text-brand-500 bg-brand-600/10"
              : "text-text-dim hover:text-text-muted"
          }`}
        >
          Host
        </button>
        {isSandboxed && (
          <button
            onClick={() => setShellMode("container")}
            className={`text-[12px] px-2 py-0.5 rounded cursor-pointer transition-colors ${
              shellMode === "container"
                ? "text-brand-500 bg-brand-600/10"
                : "text-text-dim hover:text-text-muted"
            }`}
          >
            Container
          </button>
        )}
      </div>

      {sessionId ? (
        <PairedTerminal
          key={`${sessionId}-${shellMode}`}
          sessionId={sessionId}
          mode={shellMode}
          fullViewport={fullViewport}
        />
      ) : (
        <div className="flex-1 flex items-center justify-center bg-surface-950 text-text-dim">
          <p className="text-xs">Select a session</p>
        </div>
      )}
    </div>
  );
}

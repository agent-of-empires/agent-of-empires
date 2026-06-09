import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTerminal } from "../hooks/useTerminal";
import { useMobileKeyboard } from "../hooks/useMobileKeyboard";
import { MobileTerminalToolbar } from "./MobileTerminalToolbar";
import { BackToLiveButton } from "./BackToLiveButton";
import { KeyboardFab } from "./KeyboardFab";
import { SwitchViewAction } from "./acp/SwitchViewAction";
import { ensureSession } from "../lib/api";
import { isAcpCapable } from "../lib/acpCapableTools";
import { safeSetItem } from "../lib/safeStorage";
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
  /** When false (the default) the switch-to-structured view pill is hidden
   *  entirely so it only shows where switching to the structured view is
   *  actually offered. */
}

const SCROLL_HINT_SEEN_KEY = "aoe-mobile-scroll-hint-seen";
const SCROLL_HINT_TIMEOUT_MS = 8000;

export function TerminalView({ session, active = true }: Props) {
  const [ensureState, setEnsureState] = useState<"pending" | "ready" | "error">("pending");
  const [ensureError, setEnsureError] = useState<string | null>(null);
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
  } = useTerminal(ensureState === "ready" ? session.id : null, "ws", active, session.claude_fullscreen, active);
  const { isMobile, keyboardOpen, keyboardOcclusion } = useMobileKeyboard();
  const [trackedSessionId, setTrackedSessionId] = useState(session.id);
  if (session.id !== trackedSessionId) {
    setTrackedSessionId(session.id);
    setEnsureState("pending");
    setEnsureError(null);
  }
  const lastEnsuredSessionIdRef = useRef<string | null>(null);
  const [ctrlActive, setCtrlActive] = useState(false);
  const [termFocused, setTermFocused] = useState(false);
  // On mobile, pad the viewport by the live keyboard occlusion so the
  // terminal pane shrinks while the soft keyboard is up and grows back when
  // it dismisses. On desktop keyboardOcclusion stays 0 and this is a no-op.
  const appliedKeyboardPadding = keyboardOcclusion;

  const focusSelf = useCallback(() => {
    const ta = termRef.current?.element?.querySelector("textarea");
    if (ta instanceof HTMLElement) {
      ta.focus();
      return true;
    }
    return false;
  }, [termRef]);

  // Sync React state → hook ref in an effect. The mobile toolbar toggles
  // `ctrlActive` but the onData callback reads the ref to decide whether
  // to transform the next keystroke. Writing refs during render trips
  // react-hooks/refs; a commit-phase effect does the same work without
  // tripping the lint.
  useEffect(() => {
    ctrlActiveRef.current = ctrlActive;
  });
  useEffect(() => {
    clearCtrlRef.current = () => setCtrlActive(false);
  }, [clearCtrlRef]);

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

  const [hintDismissed, setHintDismissed] = useState(() => {
    try {
      return localStorage.getItem(SCROLL_HINT_SEEN_KEY) === "1";
    } catch {
      return true;
    }
  });
  const showScrollHint = isMobile && state.connected && !hintDismissed;

  // Padding changes on the parent (first keyboard open, orientation
  // flip, fullscreen toggle) shrink the terminal container. The hook's
  // ResizeObserver picks the new size up and refits the grid; a debounced
  // window resize event nudges anything else that's watching layout.
  useLayoutEffect(() => {
    const t = setTimeout(() => {
      window.dispatchEvent(new Event("resize"));
    }, 150);
    return () => clearTimeout(t);
  }, [appliedKeyboardPadding]);

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

  // Auto-keyboard-open on initial connect removed (#1178): the
  // KeyboardFab is always visible and lets the user open the keyboard
  // explicitly. Popping the soft keyboard on every session open is the
  // wrong default for navigation, especially read-only check-ins.

  // Toggle keyboard: focus/blur MUST be the first thing in this handler
  // so iOS considers it part of the user-gesture chain. Anything before
  // focus() (even a synchronous ws.send) can break iOS keyboard display.
  // Claim primary after the focus so the PTY resizes to this viewport.
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

  // Dismiss scroll hint on first touch or timeout.
  useEffect(() => {
    if (!showScrollHint) return;
    const markSeen = () => {
      setHintDismissed(true);
      safeSetItem(SCROLL_HINT_SEEN_KEY, "1");
    };
    const t = setTimeout(markSeen, SCROLL_HINT_TIMEOUT_MS);
    const c = containerRef.current;
    c?.addEventListener("touchmove", markSeen, { once: true });
    return () => {
      clearTimeout(t);
      c?.removeEventListener("touchmove", markSeen);
    };
  }, [showScrollHint, containerRef]);

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

  // Pad the viewport by the live keyboard occlusion so the pane tracks the
  // soft keyboard: it shrinks when the keyboard opens and grows back when it
  // closes. The hook debounces the occlusion so one open/close is one PTY
  // resize, not a storm during the keyboard animation.
  const rootStyle = {
    paddingBottom: appliedKeyboardPadding > 0 ? appliedKeyboardPadding : undefined,
  } as const;
  return (
    <div className="flex-1 flex flex-col overflow-hidden relative md:bg-surface-800 md:pb-1.5" style={rootStyle}>
      {/* Top-right view switch, a discreet pill that lets the
          user flip this session into structured view mode. Only enabled
          for tools whose ACP adapter we ship. */}
      {session?.id && (
        <div className="absolute right-2 top-2 z-10">
          <SwitchViewAction
            sessionId={session.id}
            structuredView={false}
            acpCapable={isAcpCapable(session.tool, session.acp_capable)}
            variant="icon"
          />
        </div>
      )}
      {!state.connected && state.reconnecting && (
        <div className="bg-status-waiting/15 border-b border-status-waiting/30 px-4 py-1.5 flex items-center gap-2 shrink-0">
          <span className="text-xs text-status-waiting">
            Reconnecting in {state.retryCountdown}s... ({state.retryCount}/{maxRetries})
          </span>
        </div>
      )}
      {!state.connected && !state.reconnecting && state.retryCount >= maxRetries && (
        <div className="bg-status-error/10 border-b border-status-error/30 px-4 py-1.5 flex items-center gap-2 shrink-0">
          <span className="text-xs text-status-error">Connection lost</span>
          <button
            onClick={manualReconnect}
            className="text-xs text-brand-500 hover:text-brand-400 cursor-pointer underline"
          >
            Retry
          </button>
        </div>
      )}

      <div
        data-term="agent"
        className={`flex-1 overflow-hidden bg-surface-950 relative md:rounded-lg term-panel${termFocused ? " term-focused" : ""}`}
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
              Viewing from another device. Tap to take over.
            </span>
          </div>
        )}

        {showScrollHint && (
          <div
            aria-hidden="true"
            className="absolute left-0 right-0 top-3 flex justify-center pointer-events-none motion-safe:animate-[fadeIn_300ms_ease-out]"
          >
            <span className="flex items-center gap-2 font-mono text-[13px] text-text-primary bg-surface-800/95 border border-surface-700 rounded-md px-3 py-2 shadow-lg backdrop-blur-sm">
              <span aria-hidden="true" className="text-base leading-none">
                {"\u21C5"}
              </span>
              Swipe to scroll
            </span>
          </div>
        )}

        {isMobile && state.isInScrollback && <BackToLiveButton onClick={exitScrollback} topOffset="top-3" />}

        {isMobile && state.connected && <KeyboardFab keyboardOpen={keyboardOpen} onToggle={toggleKeyboard} />}
      </div>

      {isMobile && state.connected && (
        <MobileTerminalToolbar
          sendData={sendData}
          termRef={termRef}
          keyboardOpen={keyboardOpen}
          parentHandlesKeyboardInset
          ctrlActive={ctrlActive}
          onCtrlToggle={() => setCtrlActive((v) => !v)}
        />
      )}
    </div>
  );
}

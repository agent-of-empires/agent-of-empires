import { useCallback, useEffect, useRef, useState } from "react";
import { useLiveTerminal } from "../hooks/useLiveTerminal";
import { useMobileKeyboard } from "../hooks/useMobileKeyboard";
import { MobileTerminalToolbar } from "./MobileTerminalToolbar";
import { MobileLiveTerminal } from "./MobileLiveTerminal";
import { KeyboardFab } from "./KeyboardFab";
import { ensureSession } from "../lib/api";
import type { SessionResponse } from "../lib/types";
import {
  FOCUS_TERMINAL_EVENT,
  consumePendingTerminalFocus,
  setPendingTerminalFocus,
  type FocusTerminalDetail,
} from "../lib/terminalFocus";

interface Props {
  session: SessionResponse;
  active?: boolean;
}

/**
 * Touch-device agent terminal: chrome around the capture-snapshot live
 * pane (MobileLiveTerminal). Deliberately carries NONE of the xterm-era
 * keyboard machinery: there is no PTY to protect from SIGWINCH storms,
 * so the soft keyboard is handled by letting the layout shrink naturally
 * (`100dvh` shrinks with the keyboard on iOS PWA / iOS 26 / Android; the
 * App root pin is dropped for live sessions) plus a visualViewport-based
 * bottom inset for iOS regular Safari, where the layout viewport does
 * not shrink. The pane re-pins itself to the bottom when its container
 * resizes, which is all a bottom-anchored chat-style surface needs.
 */
export function LiveTerminalView({ session, active = true }: Props) {
  const [ensureState, setEnsureState] = useState<"pending" | "ready" | "error">("pending");
  const [ensureError, setEnsureError] = useState<string | null>(null);
  const live = useLiveTerminal(ensureState === "ready" ? session.id : null);
  const { keyboardOpen, keyboardHeight } = useMobileKeyboard();
  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  const [ctrlActive, setCtrlActive] = useState(false);
  const ctrlActiveRef = useRef(false);
  useEffect(() => {
    ctrlActiveRef.current = ctrlActive;
  }, [ctrlActive]);

  const [trackedSessionId, setTrackedSessionId] = useState(session.id);
  if (session.id !== trackedSessionId) {
    setTrackedSessionId(session.id);
    setEnsureState("pending");
    setEnsureError(null);
  }
  const lastEnsuredSessionIdRef = useRef<string | null>(null);

  const focusSelf = useCallback(() => {
    const ta = inputRef.current;
    if (ta) {
      ta.focus();
      return true;
    }
    return false;
  }, []);

  useEffect(() => {
    if (lastEnsuredSessionIdRef.current === session.id) {
      if (consumePendingTerminalFocus("agent")) focusSelf();
      return;
    }
    const controller = new AbortController();
    ensureSession(session.id, controller.signal).then((res) => {
      if (controller.signal.aborted) return;
      if (res.ok) {
        lastEnsuredSessionIdRef.current = session.id;
        setEnsureState("ready");
      } else {
        setEnsureState("error");
        setEnsureError(res.message ?? "Could not start session.");
      }
    });
    return () => controller.abort();
  }, [session.id, focusSelf]);

  // Drain a pending agent-focus latch once the pane is mounted.
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (ensureState !== "ready") return;
    if (consumePendingTerminalFocus("agent")) focusSelf();
  }, [ensureState, focusSelf]);

  // Cmd+` shortcut focuses this terminal when "agent" is the target.
  useEffect(() => {
    const onFocusEvent = (e: Event) => {
      const detail = (e as CustomEvent<FocusTerminalDetail>).detail;
      if (detail?.target !== "agent") return;
      if (!focusSelf()) setPendingTerminalFocus("agent");
    };
    window.addEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
    return () => window.removeEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
  }, [focusSelf]);

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
        } else {
          setEnsureState("error");
          setEnsureError(res.message ?? "Could not start session.");
        }
      });
      return "pending";
    });
  }, [session.id]);

  // Focus/blur MUST be first in the handler so iOS keeps the user-gesture
  // chain and actually shows the keyboard.
  const toggleKeyboard = useCallback(() => {
    const ta = inputRef.current;
    if (!ta) return;
    if (keyboardOpen) ta.blur();
    else ta.focus();
  }, [keyboardOpen]);

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

  // iOS regular Safari is the one platform where the layout viewport
  // does NOT shrink with the keyboard; inset the pane by the measured
  // keyboard height there. Everywhere else this is 0 and dvh shrink
  // does the work.
  const rootStyle = keyboardHeight > 0 ? { paddingBottom: keyboardHeight } : undefined;

  return (
    <div className="flex-1 flex flex-col overflow-hidden relative" style={rootStyle} data-term="agent">
      {!live.state.connected && live.state.reconnecting && (
        <div className="bg-status-waiting/15 border-b border-status-waiting/30 px-4 py-1.5 flex items-center gap-2 shrink-0">
          <span className="text-xs text-status-waiting">
            Reconnecting in {live.state.retryCountdown}s... ({live.state.retryCount}/{live.maxRetries})
          </span>
        </div>
      )}
      {!live.state.connected && !live.state.reconnecting && live.state.retryCount >= live.maxRetries && (
        <div className="bg-status-error/10 border-b border-status-error/30 px-4 py-1.5 flex items-center gap-2 shrink-0">
          <span className="text-xs text-status-error">Connection lost</span>
          <button
            onClick={live.manualReconnect}
            className="text-xs text-brand-500 hover:text-brand-400 cursor-pointer underline"
          >
            Retry
          </button>
        </div>
      )}

      <div className="flex-1 overflow-hidden bg-surface-950 relative">
        <MobileLiveTerminal
          frame={live.state.frame}
          connected={live.state.connected}
          active={active}
          reading={live.state.reading}
          sendResize={live.sendResize}
          setWindow={live.setWindow}
          setCadence={live.setCadence}
          enterReading={live.enterReading}
          returnToLive={live.returnToLive}
          sendData={live.sendData}
          ctrlActiveRef={ctrlActiveRef}
          clearCtrl={() => setCtrlActive(false)}
          inputRef={inputRef}
        />
        {live.state.connected && <KeyboardFab keyboardOpen={keyboardOpen} onToggle={toggleKeyboard} />}
      </div>

      {live.state.connected && (
        <MobileTerminalToolbar
          sendData={live.sendData}
          termRef={{ current: null }}
          inputElRef={inputRef}
          keyboardOpen={keyboardOpen}
          parentHandlesKeyboardInset
          ctrlActive={ctrlActive}
          onCtrlToggle={() => setCtrlActive((v) => !v)}
        />
      )}
    </div>
  );
}

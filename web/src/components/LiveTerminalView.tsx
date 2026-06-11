import { useCallback, useEffect, useRef, useState } from "react";
import { useLiveTerminal } from "../hooks/useLiveTerminal";
import { useMobileKeyboard } from "../hooks/useMobileKeyboard";
import { MobileTerminalToolbar } from "./MobileTerminalToolbar";
import { MobileLiveTerminal } from "./MobileLiveTerminal";
import { KeyboardFab } from "./KeyboardFab";
import { TerminalConnectionBanners } from "./TerminalConnectionBanners";
import { ensureSession, ensureTerminal } from "../lib/api";
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
  /** Which tmux surface this view renders. The agent pane is the
   *  default; the paired host/container shells reuse the same chrome
   *  with their own WS route, ensure call, and focus target. */
  surface?: "agent" | "paired-host" | "paired-container";
}

const SURFACES = {
  agent: { wsPath: "live-ws", focusTarget: "agent" as const, dataTerm: "agent" },
  "paired-host": { wsPath: "terminal/live-ws", focusTarget: "paired" as const, dataTerm: "paired" },
  "paired-container": {
    wsPath: "container-terminal/live-ws",
    focusTarget: "paired" as const,
    dataTerm: "paired",
  },
};

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
export function LiveTerminalView({ session, active = true, surface = "agent" }: Props) {
  const { wsPath, focusTarget, dataTerm } = SURFACES[surface];
  const [ensureState, setEnsureState] = useState<"pending" | "ready" | "error">("pending");
  const [ensureError, setEnsureError] = useState<string | null>(null);
  const live = useLiveTerminal(ensureState === "ready" ? session.id : null, wsPath);
  // Only the iOS-regular-Safari bottom inset comes from the viewport
  // hook. Keyboard open/closed state does NOT: on a touch device the
  // keyboard is open exactly when our input has focus, and focus events
  // are ground truth where viewport-occlusion heuristics misread.
  const { keyboardHeight } = useMobileKeyboard();
  const [inputFocused, setInputFocused] = useState(false);
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
      if (consumePendingTerminalFocus(focusTarget)) focusSelf();
      return;
    }
    const controller = new AbortController();
    const ensure =
      surface === "agent"
        ? ensureSession(session.id, controller.signal)
        : ensureTerminal(session.id, surface === "paired-container").then((ok) => ({
            ok,
            message: null as string | null,
          }));
    ensure.then((res) => {
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
  }, [session.id, focusSelf, surface, focusTarget]);

  // Drain a pending focus latch once the pane is mounted.
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (ensureState !== "ready") return;
    if (consumePendingTerminalFocus(focusTarget)) focusSelf();
  }, [ensureState, focusSelf, focusTarget]);

  // Cmd+` shortcut focuses this terminal when it is the dispatched target.
  useEffect(() => {
    const onFocusEvent = (e: Event) => {
      const detail = (e as CustomEvent<FocusTerminalDetail>).detail;
      if (detail?.target !== focusTarget) return;
      if (!focusSelf()) setPendingTerminalFocus(focusTarget);
    };
    window.addEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
    return () => window.removeEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
  }, [focusSelf, focusTarget]);

  const retryEnsure = useCallback(() => {
    setEnsureState((prev) => {
      if (prev === "pending") return prev;
      setEnsureError(null);
      const controller = new AbortController();
      const ensure =
        surface === "agent"
          ? ensureSession(session.id, controller.signal)
          : ensureTerminal(session.id, surface === "paired-container").then((ok) => ({
              ok,
              message: null as string | null,
            }));
      ensure.then((res) => {
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
  }, [session.id, surface]);

  // Focus/blur MUST be first in the handler so iOS keeps the user-gesture
  // chain and actually shows the keyboard.
  const toggleKeyboard = useCallback(() => {
    const ta = inputRef.current;
    if (!ta) return;
    if (inputFocused) ta.blur();
    else ta.focus();
  }, [inputFocused]);

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
    <div className="flex-1 flex flex-col overflow-hidden relative" style={rootStyle} data-term={dataTerm}>
      <TerminalConnectionBanners
        connected={live.state.connected}
        reconnecting={live.state.reconnecting}
        retryCount={live.state.retryCount}
        retryCountdown={live.state.retryCountdown}
        maxRetries={live.maxRetries}
        onRetry={live.manualReconnect}
      />

      <div className="flex-1 overflow-hidden bg-[var(--term-bg)] relative">
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
          onInputFocusChange={setInputFocused}
        />
        {live.state.connected && <KeyboardFab keyboardOpen={inputFocused} onToggle={toggleKeyboard} />}
      </div>

      {live.state.connected && (
        <MobileTerminalToolbar
          sendData={live.sendData}
          inputElRef={inputRef}
          keyboardOpen={inputFocused}
          ctrlActive={ctrlActive}
          onCtrlToggle={() => setCtrlActive((v) => !v)}
        />
      )}
    </div>
  );
}

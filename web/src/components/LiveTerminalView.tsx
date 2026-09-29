import { useCallback, useEffect, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { useIsCoarsePointer } from "../hooks/useIsCoarsePointer";
import { useLiveTerminal } from "../hooks/useLiveTerminal";
import { useMobileKeyboard } from "../hooks/useMobileKeyboard";
import { MobileTerminalToolbar } from "./MobileTerminalToolbar";
import { TerminalComposeSheet } from "./TerminalComposeSheet";
import { MobileLiveTerminal } from "./MobileLiveTerminal";
import { KeyboardFab } from "./KeyboardFab";
import { ArrowJoystick } from "./ArrowJoystick";
import { useWebSettings } from "../hooks/useWebSettings";
import { invalidateRetainedImeContext } from "../lib/mobileKeyboardProxy";
import { TerminalConnectionBanners } from "./TerminalConnectionBanners";
import { ensureSession, ensureTerminal, isStartRefusal, pasteImage } from "../lib/api";
import { armClipboardWrite, writeClipboard } from "../lib/clipboard";
import type { ArmedClipboardWrite } from "../lib/clipboard";
import type { SessionResponse } from "../lib/types";
import { reportError } from "../lib/toastBus";
import {
  FOCUS_TERMINAL_EVENT,
  consumePendingTerminalFocus,
  setPendingTerminalFocus,
  type FocusTerminalDetail,
} from "../lib/terminalFocus";
import { StrokeIcon } from "./icons";

interface Props {
  session: SessionResponse;
  active?: boolean;
  /** Which tmux surface this view renders. */
  surface?: "agent" | "paired-host" | "paired-container";
  /** Paired-terminal instance index for the tabbed terminal groups (#2437).
   *  Ignored for the agent surface; 0 is the primary paired shell. */
  terminalIndex?: number;
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

/** Touch-device agent terminal: chrome around the capture-snapshot live pane (MobileLiveTerminal). */
export function LiveTerminalView({ session, active = true, surface = "agent", terminalIndex = 0 }: Props) {
  const base = SURFACES[surface];
  const { focusTarget, dataTerm } = base;
  // Paired terminals carry their instance index as a query param so the
  // server attaches the right tmux session; the agent surface ignores it.
  const wsPath = surface === "agent" ? base.wsPath : `${base.wsPath}?index=${terminalIndex}`;
  // Touch-only chrome (the soft-keyboard toolbar and its toggle FAB) is pointless with a physical keyboard, so it
  // stays off fine-pointer devices now that this view also renders on desktop.
  const coarse = useIsCoarsePointer();
  const [ensureState, setEnsureState] = useState<"pending" | "ready" | "error">("pending");
  const [ensureWarning, setEnsureWarning] = useState<string | null>(null);
  const [ensureError, setEnsureError] = useState<string | null>(null);
  // An archived or trashed session stays refused until unarchived or restored, so Retry is pointless.
  const [ensureRetryable, setEnsureRetryable] = useState(true);
  const clipboardArmRef = useRef<ArmedClipboardWrite | null>(null);
  const receiveAgentClipboard = useCallback((text: string) => {
    const armed = clipboardArmRef.current;
    clipboardArmRef.current = null;
    if (!armed?.resolve(text)) void writeClipboard(text);
  }, []);
  const armAgentClipboard = useCallback(() => {
    clipboardArmRef.current?.cancel();
    clipboardArmRef.current = armClipboardWrite();
  }, []);
  useEffect(
    () => () => {
      clipboardArmRef.current?.cancel();
      clipboardArmRef.current = null;
    },
    [],
  );
  const live = useLiveTerminal(ensureState === "ready" ? session.id : null, wsPath, receiveAgentClipboard);
  // The viewport hook supplies the iOS-regular-Safari bottom inset and the occlusion-based keyboardOpen used to
  // gate the pane's sizing latch (occlusion is what shrinks the container, whichever element is focused).
  const { keyboardHeight, keyboardOpen } = useMobileKeyboard();
  const [inputFocused, setInputFocused] = useState(false);
  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  const [ctrlActive, setCtrlActive] = useState(false);
  const ctrlActiveRef = useRef(false);
  const clearCtrl = useCallback(() => setCtrlActive(false), []);
  useEffect(() => {
    ctrlActiveRef.current = ctrlActive;
  }, [ctrlActive]);
  const [composeOpen, setComposeOpen] = useState(false);

  const notOwner = live.state.ownerKnown && !live.state.isOwner;
  const lastUndeliveredToastRef = useRef(0);
  // Throttled, so a held repeat key does not stack toasts.
  const reportUndelivered = useCallback(() => {
    const now = Date.now();
    if (now - lastUndeliveredToastRef.current < 2000) return;
    lastUndeliveredToastRef.current = now;
    reportError(
      notOwner
        ? "Not sent: this session is live on another device. Take over first."
        : "Not sent: terminal not connected.",
    );
  }, [notOwner]);
  const { sendData, sendPaste } = live;
  // Explicit sends (toolbar, paste, compose) report a drop; typed keys stay quiet.
  const sendDataOrReport = useCallback(
    (data: string) => {
      const ok = sendData(data);
      if (!ok) reportUndelivered();
      return ok;
    },
    [sendData, reportUndelivered],
  );
  const sendPasteOrReport = useCallback(
    (text: string, submit: boolean) => {
      const ok = sendPaste(text, submit);
      if (!ok) reportUndelivered();
      return ok;
    },
    [sendPaste, reportUndelivered],
  );
  // Out of band like the toolbar keys, so the retained IME syllable stops shadowing the line.
  const sendArrow = useCallback(
    (sequence: string) => {
      invalidateRetainedImeContext(inputRef.current);
      sendDataOrReport(sequence);
    },
    [sendDataOrReport],
  );
  const submitCompose = useCallback(
    (text: string, submit: boolean) => {
      invalidateRetainedImeContext(inputRef.current);
      return sendPasteOrReport(text, submit);
    },
    [sendPasteOrReport],
  );
  const { settings: webSettings } = useWebSettings();
  const openCompose = useCallback(() => flushSync(() => setComposeOpen(true)), []);
  const closeCompose = useCallback((refocusTerminal: boolean) => {
    // Focus first, inside the tap, so iOS keeps the keyboard up for the terminal.
    if (refocusTerminal) inputRef.current?.focus();
    setComposeOpen(false);
  }, []);

  const [trackedSessionId, setTrackedSessionId] = useState(session.id);
  if (session.id !== trackedSessionId) {
    setTrackedSessionId(session.id);
    setComposeOpen(false);
    setEnsureState("pending");
    setEnsureWarning(null);
    setEnsureError(null);
  }
  const lastEnsuredSessionIdRef = useRef<string | null>(null);

  const uploadPastedImage = useCallback((file: File) => pasteImage(session.id, file), [session.id]);

  const focusSelf = useCallback(() => {
    const ta = inputRef.current;
    if (ta) {
      ta.focus();
      return true;
    }
    return false;
  }, []);

  // A refused ensure re-runs once the session is unarchived or restored.
  const shelved = !!session.archived_at || !!session.trashed_at;
  useEffect(() => {
    if (lastEnsuredSessionIdRef.current === session.id) {
      if (consumePendingTerminalFocus(focusTarget)) focusSelf();
      return;
    }
    const controller = new AbortController();
    const ensure =
      surface === "agent"
        ? ensureSession(session.id, controller.signal)
        : ensureTerminal(session.id, terminalIndex, surface === "paired-container");
    ensure.then((res) => {
      if (controller.signal.aborted) return;
      if (res.ok) {
        lastEnsuredSessionIdRef.current = session.id;
        setEnsureWarning(res.message ?? null);
        setEnsureState("ready");
      } else {
        setEnsureState("error");
        setEnsureError(res.message ?? "Could not start session.");
        setEnsureRetryable(!isStartRefusal(res.error));
      }
    });
    return () => controller.abort();
  }, [session.id, shelved, focusSelf, surface, focusTarget, terminalIndex]);

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
          : ensureTerminal(session.id, terminalIndex, surface === "paired-container");
      ensure.then((res) => {
        if (controller.signal.aborted) return;
        if (res.ok) {
          lastEnsuredSessionIdRef.current = session.id;
          setEnsureWarning(res.message ?? null);
          setEnsureState("ready");
        } else {
          setEnsureState("error");
          setEnsureError(res.message ?? "Could not start session.");
          setEnsureRetryable(!isStartRefusal(res.error));
        }
      });
      return "pending";
    });
  }, [session.id, surface, terminalIndex]);

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
        {ensureRetryable && (
          <button
            onClick={retryEnsure}
            className="text-xs text-brand-500 hover:text-brand-400 cursor-pointer underline"
          >
            Retry
          </button>
        )}
      </div>
    );
  }

  // Keyboard-open lift only.
  const rootStyle = keyboardHeight > 0 ? { paddingBottom: keyboardHeight } : undefined;

  return (
    <div
      className="flex-1 flex flex-col overflow-hidden relative"
      style={rootStyle}
      data-term={dataTerm}
      data-pane-focused={inputFocused || undefined}
    >
      {/* Frame the pane like the TUI does: a faint always-on border marks the box edges and brightens to the teal
         `terminal-active` color when this pane is selected (its input has focus), so on a multi-pane desktop it
         is obvious which box keystrokes go to. */}
      <div
        aria-hidden="true"
        className={`pointer-events-none absolute inset-0 z-10 ring-inset transition-shadow ${
          coarse ? "" : inputFocused ? "ring-2 ring-terminal-active" : "ring-1 ring-surface-700/40"
        }`}
      />

      <TerminalConnectionBanners
        connected={live.state.connected}
        reconnecting={live.state.reconnecting}
        retryCount={live.state.retryCount}
        retryCountdown={live.state.retryCountdown}
        maxRetries={live.maxRetries}
        onRetry={live.manualReconnect}
      />

      {ensureWarning && (
        <div className="absolute left-0 right-0 top-12 z-20 flex justify-center px-3 pointer-events-none">
          <span className="text-xs text-status-warning bg-surface-900/90 border border-surface-700/60 rounded-full px-4 py-2 max-w-md break-words text-center">
            {ensureWarning}
          </span>
        </div>
      )}

      {live.state.connected && live.state.ownerKnown && !live.state.isOwner && (
        <div className="absolute left-0 right-0 top-3 flex justify-center z-20 px-3">
          <button
            type="button"
            onClick={live.claim}
            data-live-takeover
            className="flex items-center gap-1.5 text-xs font-semibold text-white bg-brand-600 hover:bg-brand-500 active:bg-brand-700 border border-brand-400/50 rounded-full px-4 py-2 shadow-lg cursor-pointer animate-fade-in"
          >
            <StrokeIcon size={13} strokeWidth="2.5" hidden>
              <path d="M9 18l6-6-6-6" />
            </StrokeIcon>
            Live on another device. Take over
          </button>
        </div>
      )}

      <div
        className="flex-1 overflow-hidden bg-[var(--term-bg)] relative"
        // Click-to-type, like every terminal.
        onClick={() => {
          if (coarse) return;
          const sel = window.getSelection();
          if (sel && !sel.isCollapsed) return;
          focusSelf();
        }}
      >
        <MobileLiveTerminal
          frame={live.state.frame}
          liveStats={live.state.stats}
          transport={live.state.transport}
          armAgentClipboard={armAgentClipboard}
          connected={live.state.connected}
          active={active}
          reading={live.state.reading}
          sendResize={live.sendResize}
          setWindow={live.setWindow}
          setCadence={live.setCadence}
          enterReading={live.enterReading}
          returnToLive={live.returnToLive}
          sendData={live.sendData}
          sendPaste={sendPasteOrReport}
          uploadPastedImage={uploadPastedImage}
          forwardWheel={live.forwardWheel}
          forwardButton={live.forwardButton}
          ctrlActiveRef={ctrlActiveRef}
          clearCtrl={clearCtrl}
          inputRef={inputRef}
          onInputFocusChange={setInputFocused}
          bottomAlign={surface === "agent"}
          keyboardOpen={keyboardOpen}
        />
        {coarse && live.state.connected && <KeyboardFab keyboardOpen={inputFocused} onToggle={toggleKeyboard} />}
        {coarse && live.state.connected && webSettings.showArrowJoystick && <ArrowJoystick onArrow={sendArrow} />}
      </div>

      {coarse && live.state.connected && (
        <MobileTerminalToolbar
          keys={webSettings.mobileToolbarKeys}
          sendData={sendDataOrReport}
          sendPaste={sendPasteOrReport}
          onCompose={openCompose}
          inputElRef={inputRef}
          keyboardOpen={inputFocused}
          compact={!keyboardOpen && !inputFocused}
          ctrlActive={ctrlActive}
          onCtrlToggle={() => setCtrlActive((v) => !v)}
        />
      )}

      {composeOpen && (
        <TerminalComposeSheet
          draftKey={`${session.id}:${surface}:${terminalIndex}`}
          bottomInset={keyboardHeight}
          onSubmit={submitCompose}
          onClose={closeCompose}
        />
      )}
    </div>
  );
}

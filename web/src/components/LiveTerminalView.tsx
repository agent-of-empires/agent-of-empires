import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
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
import {
  FOCUS_TERMINAL_EVENT,
  consumePendingTerminalFocus,
  focusIntentMatches,
  setPendingTerminalFocus,
  type FocusTerminalDetail,
} from "../lib/terminalFocus";
import { safeGetItem, safeRemoveItem, safeSetItem } from "../lib/safeStorage";
import { isSessionActive } from "../lib/session";
import { publishLivePane } from "../lib/liveControl";
import { LiveControlStrip } from "./LiveControlStrip";

interface Props {
  readOnly: boolean;
  session: SessionResponse;
  active?: boolean;
  /** Which tmux surface this view renders. */
  surface?: "agent" | "paired-host" | "paired-container";
  /** Paired-terminal instance index for the tabbed terminal groups (#2437).
   *  Ignored for the agent surface; 0 is the primary paired shell. */
  terminalIndex?: number;
}

/** How long a blocked key's "Not sent" text holds; each further blocked key restarts it. */
export const BLOCKED_HOLD_MS = 2000;
/** Past the fit's resize debounce: a view that reports no new grid by then claims with its last one. */
const CLAIM_FIT_DEADLINE_MS = 1000;
/** A claim the Daemon doesn't grant by then brings the strip back. */
const CLAIMING_TIMEOUT_MS = 5000;

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
export function LiveTerminalView({ session, readOnly, active = true, surface = "agent", terminalIndex = 0 }: Props) {
  const base = SURFACES[surface];
  const { focusTarget, dataTerm } = base;
  // Two docks can each show a paired terminal, so paired focus intents name the one they mean.
  const focusIndex = surface === "agent" ? undefined : terminalIndex;
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
  // Each surface is its own pane, so a grid on one must not clear another's dismissal. Callers key this view by
  // session, surface and index, so the key is fixed for its lifetime.
  const pipeNoticeKey = `aoe-live-pipe-notice-dismissed:${session.id}:${surface}:${terminalIndex}`;
  const [pipeNoticeDismissed, setPipeNoticeDismissed] = useState(() => safeGetItem(pipeNoticeKey) !== null);
  const dismissPipeNotice = useCallback(() => {
    safeSetItem(pipeNoticeKey, "1");
    setPipeNoticeDismissed(true);
  }, [pipeNoticeKey]);
  const clearPipeNoticeDismissal = useCallback(() => {
    // Another tab may have stored a dismissal this view never saw.
    if (safeGetItem(pipeNoticeKey) !== null) safeRemoveItem(pipeNoticeKey);
    if (pipeNoticeDismissed) setPipeNoticeDismissed(false);
  }, [pipeNoticeDismissed, pipeNoticeKey]);
  const stripCollapseKey = `aoe-live-control-strip-collapsed:${session.id}:${surface}:${terminalIndex}`;
  const [stripCollapsed, setStripCollapsed] = useState(() => safeGetItem(stripCollapseKey) !== null);
  const collapseStrip = useCallback(() => {
    safeSetItem(stripCollapseKey, "1");
    setStripCollapsed(true);
  }, [stripCollapseKey]);
  const expandStrip = useCallback(() => {
    safeRemoveItem(stripCollapseKey);
    setStripCollapsed(false);
  }, [stripCollapseKey]);
  // Read-only live routes observe existing panes; their backend fences forbid creation, claims and resizing.
  const live = useLiveTerminal(
    readOnly || ensureState === "ready" ? session.id : null,
    readOnly,
    wsPath,
    receiveAgentClipboard,
    clearPipeNoticeDismissal,
  );
  // The viewport hook supplies the iOS-regular-Safari bottom inset and the occlusion-based keyboardOpen used to
  // gate the pane's sizing latch (occlusion is what shrinks the container, whichever element is focused).
  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  const { keyboardHeight, keyboardOpen } = useMobileKeyboard(inputRef);
  const [inputFocused, setInputFocused] = useState(false);
  const policyRef = useRef({ readOnly, sessionId: session.id, surface, terminalIndex });
  useLayoutEffect(() => {
    policyRef.current = { readOnly, sessionId: session.id, surface, terminalIndex };
    if (readOnly) inputRef.current?.blur();
  }, [readOnly, session.id, surface, terminalIndex]);
  useLayoutEffect(
    () => () => {
      policyRef.current.readOnly = true;
    },
    [],
  );
  const [ctrlActive, setCtrlActive] = useState(false);
  const ctrlActiveRef = useRef(false);
  const clearCtrl = useCallback(() => setCtrlActive(false), []);
  useEffect(() => {
    ctrlActiveRef.current = ctrlActive;
  }, [ctrlActive]);
  const [composeOpen, setComposeOpen] = useState(false);

  const notOwner = live.state.ownerKnown && !live.state.isOwner;
  const viewOnly = !readOnly && live.state.connected && notOwner;
  // Take control hides the strip first, so the claim resizes the pane once, to the grid without it.
  const { claimAfterResize } = live;
  const [claiming, setClaiming] = useState(false);
  if (claiming && (live.state.isOwner || !live.state.connected)) setClaiming(false);
  const claimingTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (claimingTimerRef.current) clearTimeout(claimingTimerRef.current);
    },
    [],
  );
  const takeControlRef = useRef<HTMLButtonElement | null>(null);
  // Where the Tab escape took focus from: the terminal's input or App's keyboard proxy.
  const escapeOriginRef = useRef<HTMLElement | null>(null);
  const takeControl = useCallback(() => {
    // The strip unmounts once claiming, so focus goes back to where it was; a touch tap restores nothing, so no
    // soft keyboard opens.
    if (document.activeElement === takeControlRef.current) {
      const origin = escapeOriginRef.current?.isConnected ? escapeOriginRef.current : null;
      (origin ?? (coarse ? null : inputRef.current))?.focus();
    }
    escapeOriginRef.current = null;
    setClaiming(true);
    claimAfterResize(CLAIM_FIT_DEADLINE_MS);
    if (claimingTimerRef.current) clearTimeout(claimingTimerRef.current);
    claimingTimerRef.current = setTimeout(() => setClaiming(false), CLAIMING_TIMEOUT_MS);
  }, [claimAfterResize, coarse]);
  const [blockedHold, setBlockedHold] = useState<string | null>(null);
  const holdTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (holdTimerRef.current) clearTimeout(holdTimerRef.current);
    },
    [],
  );
  const noteBlocked = useCallback(() => {
    setBlockedHold(
      readOnly
        ? "Not sent: this Daemon is read-only."
        : notOwner
          ? "Not sent. Take control to type."
          : "Not sent: terminal not connected.",
    );
    if (holdTimerRef.current) clearTimeout(holdTimerRef.current);
    holdTimerRef.current = setTimeout(() => {
      holdTimerRef.current = null;
      setBlockedHold(null);
    }, BLOCKED_HOLD_MS);
  }, [notOwner, readOnly]);
  // Holding control drops the hold at once: no notice may resize a terminal this tab sizes.
  const holdShown =
    blockedHold !== null && !claiming && (readOnly || notOwner || !live.state.connected) ? blockedHold : null;
  const { sendData, sendPaste, manualReconnect, reconnectAndClaim } = live;
  // Automatic redials keep the dismissal; only a reconnect the user asked for brings the notice back.
  const reconnect = useCallback(() => {
    clearPipeNoticeDismissal();
    manualReconnect();
  }, [clearPipeNoticeDismissal, manualReconnect]);
  // Explicit sends (toolbar, paste, compose) always report a drop; typed keys only when another window has
  // control or the Daemon is read-only, not while the connection banner already says why.
  const sendDataOrReport = useCallback(
    (data: string) => {
      const ok = sendData(data);
      if (!ok) noteBlocked();
      return ok;
    },
    [sendData, noteBlocked],
  );
  const sendPasteOrReport = useCallback(
    (text: string, submit: boolean) => {
      const ok = sendPaste(text, submit);
      if (!ok) noteBlocked();
      return ok;
    },
    [sendPaste, noteBlocked],
  );
  const sendTyped = useCallback(
    (data: string) => {
      const ok = sendData(data);
      if (!ok && (notOwner || readOnly)) noteBlocked();
      return ok;
    },
    [sendData, noteBlocked, notOwner, readOnly],
  );
  // While view only the terminal sends no keys, so during a hold Tab can reach Take control.
  const escapeTabToTakeControl = useCallback(() => {
    if (!viewOnly || holdShown === null || !takeControlRef.current) return false;
    escapeOriginRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    takeControlRef.current.focus();
    return true;
  }, [viewOnly, holdShown]);
  const slowWhileOwner =
    !readOnly && live.state.connected && live.state.isOwner && live.state.transportReason === "pipe_owned";
  const reconnectClaiming = useCallback(() => {
    clearPipeNoticeDismissal();
    reconnectAndClaim();
  }, [clearPipeNoticeDismissal, reconnectAndClaim]);
  useEffect(() => {
    const key = `${session.id}:${surface}:${terminalIndex}`;
    publishLivePane(key, {
      sessionId: session.id,
      surface,
      terminalIndex,
      viewOnly,
      claim: takeControl,
      slow:
        slowWhileOwner && !pipeNoticeDismissed ? { reconnect: reconnectClaiming, dismiss: dismissPipeNotice } : null,
    });
    return () => publishLivePane(key, null);
  }, [
    session.id,
    surface,
    terminalIndex,
    viewOnly,
    takeControl,
    slowWhileOwner,
    pipeNoticeDismissed,
    reconnectClaiming,
    dismissPipeNotice,
  ]);
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
    if (refocusTerminal && !policyRef.current.readOnly) inputRef.current?.focus();
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
  const captureWriteAdmission = useCallback(() => {
    const policy = policyRef.current;
    return () =>
      policyRef.current === policy &&
      !policy.readOnly &&
      policy.sessionId === session.id &&
      policy.surface === surface &&
      policy.terminalIndex === terminalIndex;
  }, [session.id, surface, terminalIndex]);

  const uploadPastedImage = useCallback(
    (file: File, canWrite: () => boolean) =>
      canWrite() ? pasteImage(session.id, file, canWrite) : Promise.resolve(null),
    [session.id],
  );

  const focusSelf = useCallback(() => {
    if (policyRef.current.readOnly) return false;
    const ta = inputRef.current;
    if (ta) {
      ta.focus();
      return true;
    }
    return false;
  }, []);

  const ensureRequestRef = useRef<AbortController | null>(null);
  // Policy changes do not erase an already dispatched result. Identity changes and unmounts do.
  useEffect(
    () => () => {
      ensureRequestRef.current?.abort();
      ensureRequestRef.current = null;
      lastEnsuredSessionIdRef.current = null;
    },
    [session.id, surface, terminalIndex],
  );

  const retryEnsure = useCallback(() => {
    const policy = policyRef.current;
    if (
      policy.readOnly ||
      policy.sessionId !== session.id ||
      policy.surface !== surface ||
      policy.terminalIndex !== terminalIndex ||
      ensureRequestRef.current
    )
      return;
    const controller = new AbortController();
    ensureRequestRef.current = controller;
    setEnsureError(null);
    setEnsureState("pending");
    const ensure =
      surface === "agent"
        ? ensureSession(session.id, controller.signal)
        : ensureTerminal(session.id, terminalIndex, surface === "paired-container", controller.signal);
    ensure
      .then((res) => {
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
      })
      .finally(() => {
        if (ensureRequestRef.current === controller) ensureRequestRef.current = null;
      });
  }, [session.id, surface, terminalIndex]);

  // A refused ensure re-runs once the session is unarchived or restored.
  const shelved = !!session.archived_at || !!session.trashed_at;
  useEffect(() => {
    const policy = policyRef.current;
    if (policy.readOnly) return;
    if (lastEnsuredSessionIdRef.current === policy.sessionId) {
      if (consumePendingTerminalFocus(focusTarget, focusIndex)) focusSelf();
      return;
    }
    retryEnsure();
  }, [session.id, shelved, readOnly, focusSelf, focusTarget, focusIndex, retryEnsure]);

  // Drain a pending focus latch once the pane is mounted.
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (readOnly || ensureState !== "ready") return;
    if (consumePendingTerminalFocus(focusTarget, focusIndex)) focusSelf();
  }, [ensureState, readOnly, focusSelf, focusTarget, focusIndex]);

  // Cmd+` shortcut focuses this terminal when it is the dispatched target.
  useEffect(() => {
    const onFocusEvent = (e: Event) => {
      const detail = (e as CustomEvent<FocusTerminalDetail>).detail;
      if (policyRef.current.readOnly || !focusIntentMatches(detail, focusTarget, focusIndex)) return;
      // Latch the request's own index, so an indexless one stays open to any paired view.
      if (!focusSelf()) setPendingTerminalFocus(focusTarget, detail.terminalIndex);
    };
    window.addEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
    return () => window.removeEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
  }, [focusSelf, focusTarget, focusIndex]);

  // Stop then Start leaves a dead pane's closed socket behind. Latch the Start edge until the 4001 close
  // arrives (either order), consume it once so a crash-looping agent cannot retrigger it, and drop it
  // once a healthy replacement socket connects, so a later exit is not auto-restarted. The old socket may still be
  // open when Start lands, so only a false to true edge clears it.
  const prevStatusRef = useRef({ id: session.id, status: session.status });
  const startedRef = useRef(false);
  const wasConnectedRef = useRef(false);
  const sessionActive = isSessionActive(session.status);
  const { paneDead, connected } = live.state;
  useEffect(() => {
    const prev = prevStatusRef.current;
    prevStatusRef.current = { id: session.id, status: session.status };
    if (readOnly || prev.id !== session.id) startedRef.current = false;
    else if (prev.status === "Stopped" && sessionActive) startedRef.current = true;
    if (connected && !wasConnectedRef.current) startedRef.current = false;
    wasConnectedRef.current = connected;
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (surface === "agent" && paneDead && startedRef.current) {
      startedRef.current = false;
      retryEnsure();
    }
  }, [session.id, session.status, sessionActive, paneDead, connected, surface, retryEnsure, readOnly]);

  // Focus/blur MUST be first in the handler so iOS keeps the user-gesture
  // chain and actually shows the keyboard.
  const toggleKeyboard = useCallback(() => {
    if (policyRef.current.readOnly) return;
    const ta = inputRef.current;
    if (!ta) return;
    if (inputFocused) ta.blur();
    else ta.focus();
  }, [inputFocused]);

  if (!readOnly && ensureState === "pending") {
    return (
      <div className="flex-1 flex items-center justify-center bg-surface-950 text-text-dim">
        <span className="text-xs">Starting session...</span>
      </div>
    );
  }

  if (!readOnly && ensureState === "error") {
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
        data-pane-frame
        className={`pointer-events-none absolute inset-0 z-10 ring-inset transition-shadow ${
          coarse ? "" : `ring-1 ${inputFocused ? "ring-terminal-active" : "ring-surface-700/40"}`
        }`}
      />

      <TerminalConnectionBanners
        connected={live.state.connected}
        reconnecting={live.state.reconnecting}
        retryCount={live.state.retryCount}
        retryCountdown={live.state.retryCountdown}
        maxRetries={live.maxRetries}
        onRetry={reconnect}
        paneDead={live.state.paneDead}
        onRestart={readOnly ? undefined : retryEnsure}
      />

      {ensureWarning && (
        <div className="absolute left-0 right-0 top-12 z-20 flex justify-center px-3 pointer-events-none">
          <span className="text-xs text-status-warning bg-surface-900/90 border border-surface-700/60 rounded-full px-4 py-2 max-w-md break-words text-center">
            {ensureWarning}
          </span>
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
          readOnly={readOnly}
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
          sendData={sendTyped}
          sendPaste={sendPasteOrReport}
          uploadPastedImage={uploadPastedImage}
          captureWriteAdmission={captureWriteAdmission}
          onTabEscape={escapeTabToTakeControl}
          forwardWheel={live.forwardWheel}
          forwardButton={live.forwardButton}
          ctrlActiveRef={ctrlActiveRef}
          clearCtrl={clearCtrl}
          inputRef={inputRef}
          onInputFocusChange={setInputFocused}
          bottomAlign={surface === "agent"}
          keyboardOpen={keyboardOpen}
        />
        {!readOnly && coarse && live.state.connected && (
          <KeyboardFab keyboardOpen={inputFocused} onToggle={toggleKeyboard} />
        )}
        {!readOnly && coarse && live.state.connected && webSettings.showArrowJoystick && (
          <ArrowJoystick onArrow={sendArrow} />
        )}
      </div>

      <LiveControlStrip
        mode={readOnly ? "read-only" : viewOnly && !claiming ? "view-only" : null}
        hold={holdShown}
        ownerKind={live.state.ownerKind}
        lag={live.state.transportReason === "pipe_owned"}
        collapsed={stripCollapsed}
        onTakeControl={takeControl}
        onCollapse={collapseStrip}
        onExpand={expandStrip}
        takeControlRef={takeControlRef}
      />

      {!readOnly && coarse && live.state.connected && (
        <MobileTerminalToolbar
          keys={webSettings.mobileToolbarKeys}
          captureWriteAdmission={captureWriteAdmission}
          sendData={sendDataOrReport}
          sendPaste={sendPasteOrReport}
          onCompose={openCompose}
          inputElRef={inputRef}
          keyboardOpen={inputFocused}
          compact={!keyboardOpen && !inputFocused}
          ctrlActive={ctrlActive}
          onCtrlToggle={() => setCtrlActive((v) => !v)}
          onCtrlClear={clearCtrl}
        />
      )}

      {composeOpen && (
        <TerminalComposeSheet
          draftKey={`${session.id}:${surface}:${terminalIndex}`}
          bottomInset={keyboardHeight}
          readOnly={readOnly}
          onSubmit={submitCompose}
          onClose={closeCompose}
        />
      )}
    </div>
  );
}

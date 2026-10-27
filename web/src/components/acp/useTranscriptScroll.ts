import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

import { useIsCoarsePointer } from "../../hooks/useIsCoarsePointer";
import { useMobileKeyboard } from "../../hooks/useMobileKeyboard";
import { loadScrollState, restoredScrollTop, saveScrollState } from "../../lib/acpScrollState";
import type { ActivityRow } from "../../lib/acpTypes";
import { anchorIsStale, autoLoadDecision, isPinnedToBottom, scrollRestoreDelta } from "../../lib/historyScroll";
import { promptRepinDecision } from "../../lib/promptRepin";
import { repinOnResize } from "../../lib/repinOnResize";
import { resampleToolAnchors, sampleToolAnchors, toolAnchorDelta, type ToolAnchor } from "../../lib/toolAnchor";

/** Stick-to-bottom, earlier-history auto-load, and PWA-reopen scroll restore
 *  for the transcript viewport. These observers own bottom-following; the
 *  viewport primitive's own auto-scroll must stay disabled. */
export function useTranscriptScroll({
  sessionId,
  canLoadEarlierHistory,
  loadEarlierHistory,
  loadingEarlierHistory,
  historyRows,
  composerCollapsed,
  promptSeq,
  hasEverOpened,
  localInflight,
}: {
  sessionId: string;
  canLoadEarlierHistory: boolean;
  loadEarlierHistory: () => void;
  loadingEarlierHistory: boolean;
  composerCollapsed: boolean;
  historyRows?: readonly ActivityRow[];
  /** Counts every prompt once, from any path or device; keys the submit re-pin. */
  promptSeq: number;
  hasEverOpened: boolean;
  /** This client has an optimistic prompt row still awaiting its server echo. */
  localInflight: boolean;
}) {
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const belowViewportRef = useRef<HTMLDivElement | null>(null);
  const messagesContentRef = useRef<HTMLDivElement | null>(null);
  // Sampled on scroll: by the time a ResizeObserver fires, layout has already
  // settled, so pinned-ness must be read before the resize.
  const wasAtBottomRef = useRef<boolean>(true);
  const lastScrollTopRef = useRef(0);
  const lastScrollHeightRef = useRef(0);
  const lastClientHeightRef = useRef(0);
  // Last time we sampled at the bottom. iOS fires an interim scroll during a
  // keyboard resize that clears `wasAtBottomRef` but cannot clear this.
  const lastAtBottomAtRef = useRef(0);
  const toolAnchorsRef = useRef<readonly ToolAnchor[]>([]);
  // A fold can remove a card before React remounts its replacement.
  const pendingFoldAnchorMissRef = useRef(false);
  // Keep stale anchors through another layout pass; a later fold commit can clamp a correction.
  const pendingFoldCorrectionStableRef = useRef(false);
  // The pin intent when the anchors were last fresh. A fold shrinks the layout under the
  // reader and the browser clamps the scroll, which would otherwise read as a re-pin.
  const anchorIntentRef = useRef(true);
  const didRestoreScrollRef = useRef(false);
  const [atBottom, setAtBottom] = useState(true);
  const { keyboardOpen } = useMobileKeyboard();
  const isCoarse = useIsCoarsePointer();

  /** An explicit "stick again": a programmatic scroll fires no gesture, so set
   *  the stick intent directly. */
  const pinToBottom = useCallback((behavior: ScrollBehavior) => {
    const vp = viewportRef.current;
    if (!vp) return;
    wasAtBottomRef.current = true;
    anchorIntentRef.current = true;
    lastAtBottomAtRef.current = performance.now();
    setAtBottom(true);
    vp.scrollTo({ top: vp.scrollHeight, behavior });
    lastScrollTopRef.current = vp.scrollTop;
  }, []);
  const scrollToBottom = useCallback(() => pinToBottom("instant"), [pinToBottom]);

  // A new prompt re-engages stick-to-bottom, as the CLI does: on a fine pointer
  // the composer growing while typing can drop the pinned intent. See
  // `promptRepinDecision` for why replayed prompts do not count.
  const seenPromptSeqRef = useRef<number | null>(null);
  useEffect(() => {
    const d = promptRepinDecision({
      seen: seenPromptSeqRef.current,
      promptSeq,
      live: hasEverOpened,
      localInflight,
    });
    seenPromptSeqRef.current = d.seen;
    if (d.pin) pinToBottom("auto");
  }, [promptSeq, hasEverOpened, localInflight, pinToBottom]);

  // Mirrors so the scroll effect sees the latest load wiring without re-subscribing.
  const canLoadEarlierRef = useRef(canLoadEarlierHistory);
  const loadEarlierRef = useRef(loadEarlierHistory);
  const loadingEarlierRef = useRef(loadingEarlierHistory);
  useEffect(() => {
    canLoadEarlierRef.current = canLoadEarlierHistory;
    loadEarlierRef.current = loadEarlierHistory;
    loadingEarlierRef.current = loadingEarlierHistory;
  }, [canLoadEarlierHistory, loadEarlierHistory, loadingEarlierHistory]);
  const autoLoadArmedRef = useRef(true);
  // Pre-growth scrollHeight, so the content observer can hold the read position
  // when older rows land above it.
  const pendingScrollAnchorRef = useRef<number | null>(null);
  const lastAutoLoadAtRef = useRef(0);

  const requestEarlierHistory = useCallback(() => {
    const vp = viewportRef.current;
    if (!vp || !canLoadEarlierRef.current) return;
    lastAutoLoadAtRef.current = performance.now();
    const stamped = vp.scrollHeight;
    pendingScrollAnchorRef.current = stamped;
    loadEarlierRef.current();
    // Drop an anchor the request did not use, or it would jump the viewport on
    // the next unrelated growth.
    requestAnimationFrame(() => {
      if (
        pendingScrollAnchorRef.current === stamped &&
        anchorIsStale(loadingEarlierRef.current, pendingScrollAnchorRef.current, vp.scrollHeight)
      ) {
        pendingScrollAnchorRef.current = null;
      }
    });
  }, []);

  useEffect(() => {
    const vp = viewportRef.current;
    if (vp && anchorIsStale(loadingEarlierHistory, pendingScrollAnchorRef.current, vp.scrollHeight)) {
      pendingScrollAnchorRef.current = null;
    }
  }, [loadingEarlierHistory]);

  const previousHistoryRef = useRef<{ sessionId: string; firstRowId: string | null }>({
    sessionId,
    firstRowId: null,
  });
  useLayoutEffect(() => {
    const previous = previousHistoryRef.current;
    if (
      previous.sessionId === sessionId &&
      previous.firstRowId !== null &&
      historyRows &&
      historyRows[0]?.id !== previous.firstRowId &&
      historyRows.some((row) => row.id === previous.firstRowId) &&
      !wasAtBottomRef.current &&
      pendingScrollAnchorRef.current === null
    ) {
      const stamped = lastScrollHeightRef.current;
      pendingScrollAnchorRef.current = stamped;
      requestAnimationFrame(() => {
        const vp = viewportRef.current;
        if (
          vp &&
          pendingScrollAnchorRef.current === stamped &&
          anchorIsStale(loadingEarlierRef.current, stamped, vp.scrollHeight)
        ) {
          pendingScrollAnchorRef.current = null;
        }
      });
    }
    previousHistoryRef.current = { sessionId, firstRowId: historyRows?.[0]?.id ?? null };
  }, [sessionId, historyRows]);
  useLayoutEffect(() => {
    const vp = viewportRef.current;
    const below = belowViewportRef.current;
    const content = messagesContentRef.current;
    if (!vp || !below) return;
    lastScrollTopRef.current = vp.scrollTop;
    lastScrollHeightRef.current = vp.scrollHeight;
    lastClientHeightRef.current = vp.clientHeight;
    // On coarse pointers, scroll events include programmatic changes. Only real input
    // gestures update the pinned state there.
    let gestureActive = false;
    let gestureClearTimer = 0;
    const scheduleGestureClear = () => {
      if (gestureClearTimer) window.clearTimeout(gestureClearTimer);
      gestureClearTimer = window.setTimeout(() => {
        gestureActive = false;
      }, 250);
    };
    const markGesture = () => {
      gestureActive = true;
      scheduleGestureClear();
    };
    const markKeyboardGesture = (event: KeyboardEvent) => {
      if (event.target !== vp) return;
      switch (event.key) {
        case "ArrowDown":
        case "ArrowUp":
        case "PageDown":
        case "PageUp":
        case "Home":
        case "End":
        case " ":
          markGesture();
      }
    };
    const sample = (force = false, updatePinned = true, allowAutoLoad = false) => {
      if (force || !isCoarse || gestureActive) {
        const pinned = isPinnedToBottom(vp.scrollTop, vp.clientHeight, vp.scrollHeight);
        if (updatePinned) {
          const prevStuck = wasAtBottomRef.current;
          wasAtBottomRef.current = pinned;
          if (pinned) lastAtBottomAtRef.current = performance.now();
          setAtBottom((prev) => (prev === pinned ? prev : pinned));
          // Gated on the restore having run: the forced mount sample reads a tall
          // transcript at scrollTop 0 as unpinned and would clobber the saved intent.
          if (pinned !== prevStuck && didRestoreScrollRef.current) {
            saveScrollState(sessionId, { stuck: pinned, top: vp.scrollTop });
          }
        }
        // Momentum scrolling fires with no fresh touchmove; keep the gesture alive.
        if (gestureActive) scheduleGestureClear();
      }
      const decision = autoLoadDecision({
        scrollTop: vp.scrollTop,
        clientHeight: vp.clientHeight,
        scrollHeight: vp.scrollHeight,
        armed: autoLoadArmedRef.current,
        canLoadEarlier: allowAutoLoad && canLoadEarlierRef.current,
        now: performance.now(),
        lastLoadAt: lastAutoLoadAtRef.current,
      });
      autoLoadArmedRef.current = decision.armed;
      if (decision.fire) requestEarlierHistory();
    };
    const onScroll = () => {
      const previous = toolAnchorsRef.current;
      const folding = previous.some(({ el }) => !el.isConnected);
      const layoutChanged =
        vp.scrollHeight !== lastScrollHeightRef.current || vp.clientHeight !== lastClientHeightRef.current;
      const scrollChanged = vp.scrollTop !== lastScrollTopRef.current;
      let readerMoved = false;
      if (scrollChanged) {
        const maxScrollTop = vp.scrollHeight - vp.clientHeight;
        const sampledScrollTop = layoutChanged
          ? Math.min(lastScrollTopRef.current, maxScrollTop)
          : lastScrollTopRef.current;
        const readerDelta = vp.scrollTop - sampledScrollTop;
        if (readerDelta !== 0) {
          readerMoved = true;
          if (folding) {
            toolAnchorsRef.current = previous.map((anchor) => ({
              ...anchor,
              offset: anchor.offset - readerDelta,
              scrollTop: vp.scrollTop,
            }));
          }
        }
      }
      sample(readerMoved, !layoutChanged || readerMoved || gestureActive, readerMoved);
      if (!folding) {
        toolAnchorsRef.current = resampleToolAnchors(vp, previous);
      }
      if (readerMoved || (scrollChanged && !layoutChanged)) {
        anchorIntentRef.current = wasAtBottomRef.current;
      }
      lastScrollTopRef.current = vp.scrollTop;
      lastScrollHeightRef.current = vp.scrollHeight;
      lastClientHeightRef.current = vp.clientHeight;
    };
    sample(true);
    vp.addEventListener("scroll", onScroll, { passive: true });
    vp.addEventListener("wheel", markGesture, { passive: true });
    vp.addEventListener("touchmove", markGesture, { passive: true });
    vp.addEventListener("keydown", markKeyboardGesture);
    // Pin on every visualViewport resize frame so the transcript tracks the
    // soft keyboard animation in lockstep.
    const vv = typeof window !== "undefined" ? window.visualViewport : null;
    const onVvResize = () => {
      if (wasAtBottomRef.current) {
        vp.scrollTop = vp.scrollHeight;
        lastScrollTopRef.current = vp.scrollTop;
      }
    };
    vv?.addEventListener("resize", onVvResize);

    if (!didRestoreScrollRef.current) {
      didRestoreScrollRef.current = true;
      const saved = loadScrollState(sessionId);
      const stick = !saved || saved.stuck;
      wasAtBottomRef.current = stick;
      anchorIntentRef.current = stick;
      if (stick) lastAtBottomAtRef.current = performance.now();
      setAtBottom(stick);
      // Later passes catch content that lays out after paint; each rechecks the
      // current stick intent so an intervening user scroll wins.
      const applyStart = () => {
        const top = restoredScrollTop(saved, wasAtBottomRef.current, vp.scrollHeight, vp.clientHeight);
        if (top != null) {
          vp.scrollTop = top;
          lastScrollTopRef.current = vp.scrollTop;
        }
      };
      applyStart();
      requestAnimationFrame(() => requestAnimationFrame(applyStart));
      if (stick) window.setTimeout(applyStart, 150);
    }

    const saveScroll = () => {
      saveScrollState(sessionId, { stuck: wasAtBottomRef.current, top: vp.scrollTop });
    };
    const onVisibility = () => {
      if (document.visibilityState === "hidden") saveScroll();
    };
    window.addEventListener("pagehide", saveScroll);
    document.addEventListener("visibilitychange", onVisibility);
    // The viewport itself is observed too: chrome outside this view (the App
    // header collapse) can resize it.
    const wasAtBottom = () => wasAtBottomRef.current;
    const repin = () => {
      vp.scrollTop = vp.scrollHeight;
      lastScrollTopRef.current = vp.scrollTop;
    };
    const ro = repinOnResize({ target: below, readHeight: () => below.offsetHeight, wasAtBottom, repin });
    const vpRo = repinOnResize({ target: vp, readHeight: () => vp.clientHeight, wasAtBottom, repin });
    // Growth with a pending anchor came from older rows at the top: keep the
    // read position. Otherwise it grew at the bottom: follow if pinned.
    const contentRo = new ResizeObserver(() => {
      // Keep pre-fold anchors across a clamp; streamed content may provide the missing scroll range later.
      const resample = () => {
        toolAnchorsRef.current = sampleToolAnchors(vp);
        anchorIntentRef.current = wasAtBottomRef.current;
        pendingFoldAnchorMissRef.current = false;
        pendingFoldCorrectionStableRef.current = false;
        lastScrollHeightRef.current = vp.scrollHeight;
        lastClientHeightRef.current = vp.clientHeight;
      };
      const anchor = pendingScrollAnchorRef.current;
      if (anchor != null) {
        const delta = scrollRestoreDelta(anchor, vp.scrollHeight, wasAtBottomRef.current);
        if (delta > 0) {
          vp.scrollTop += delta;
          lastScrollTopRef.current = vp.scrollTop;
        }
        pendingScrollAnchorRef.current = null;
        sample(false, false);
        resample();
        return;
      }
      const folding = toolAnchorsRef.current.some(({ el }) => !el.isConnected);
      if (anchorIntentRef.current && (folding || !wasAtBottomRef.current)) {
        pinToBottom("auto");
        resample();
        return;
      }
      let remountedElement: Element | null = null;
      let remountedAnchor: ToolAnchor | null = null;
      let remountedOffset: number | null = null;
      let survivingAnchor = false;
      if (folding) {
        for (const anchor of toolAnchorsRef.current) {
          if (anchor.el.isConnected) {
            survivingAnchor = true;
            break;
          }
          const next = vp.querySelector(`[data-tool-id="${CSS.escape(anchor.id)}"]`);
          if (next) {
            remountedElement = next;
            remountedOffset = anchor.offset;
            remountedAnchor = anchor;
            break;
          }
        }
        if (remountedElement === null) {
          pendingFoldCorrectionStableRef.current = false;
          if (survivingAnchor || pendingFoldAnchorMissRef.current) resample();
          else pendingFoldAnchorMissRef.current = true;
          return;
        }
      }
      pendingFoldAnchorMissRef.current = false;
      if (!folding && wasAtBottomRef.current) {
        vp.scrollTop = vp.scrollHeight;
        lastScrollTopRef.current = vp.scrollTop;
        resample();
        return;
      }
      const delta = toolAnchorDelta(vp, toolAnchorsRef.current);
      const beforeCorrection = vp.scrollTop;
      vp.scrollTop = beforeCorrection + delta;
      lastScrollTopRef.current = vp.scrollTop;
      if (folding) {
        const appliedDelta = vp.scrollTop - beforeCorrection;
        const maxScrollTop = vp.scrollHeight - vp.clientHeight;
        // Advance only our correction, so later user scrolling remains in the delta.
        const correctedScrollTop = Math.min(remountedAnchor!.scrollTop, maxScrollTop) + appliedDelta;
        toolAnchorsRef.current = toolAnchorsRef.current.map((anchor) => ({
          ...anchor,
          scrollTop: Math.min(anchor.scrollTop, maxScrollTop) + appliedDelta,
        }));
        const currentOffset = remountedElement!.getBoundingClientRect().top - vp.getBoundingClientRect().top;
        const expectedOffset = remountedOffset! - (vp.scrollTop - correctedScrollTop);
        if (Math.abs(currentOffset - expectedOffset) > 1) {
          pendingFoldCorrectionStableRef.current = false;
          return;
        }
        if (Math.abs(delta) >= 1 || !pendingFoldCorrectionStableRef.current) {
          pendingFoldCorrectionStableRef.current = true;
          return;
        }
      }
      sample();
      resample();
    });
    if (content) contentRo.observe(content);
    return () => {
      ro.disconnect();
      vpRo.disconnect();
      contentRo.disconnect();
      vp.removeEventListener("scroll", onScroll);
      vp.removeEventListener("wheel", markGesture);
      vp.removeEventListener("touchmove", markGesture);
      vp.removeEventListener("keydown", markKeyboardGesture);
      vv?.removeEventListener("resize", onVvResize);
      window.removeEventListener("pagehide", saveScroll);
      document.removeEventListener("visibilitychange", onVisibility);
      saveScroll();
      if (gestureClearTimer) window.clearTimeout(gestureClearTimer);
    };
  }, [requestEarlierHistory, isCoarse, pinToBottom, sessionId]);

  // A fold can clamp and regrow before its scroll event or resize observer runs.
  useLayoutEffect(() => {
    const vp = viewportRef.current;
    if (!vp) return;
    const maxScrollTop = vp.scrollHeight - vp.clientHeight;
    if (lastScrollTopRef.current > maxScrollTop && vp.scrollTop === maxScrollTop) {
      lastScrollTopRef.current = maxScrollTop;
    }
  });

  // Hold the bottom pin through a keyboard or composer-collapse transition.
  // `wasAtBottomRef` covers sitting idle at the bottom; the timestamp covers an
  // interim resize-scroll that already cleared the ref.
  const chromeTransitionInitRef = useRef(true);
  useEffect(() => {
    if (chromeTransitionInitRef.current) {
      chromeTransitionInitRef.current = false;
      return;
    }
    const vp = viewportRef.current;
    if (!vp) return;
    const recentlyAtBottom = performance.now() - lastAtBottomAtRef.current < 1200;
    if (!wasAtBottomRef.current && !recentlyAtBottom) return;
    let raf = 0;
    const start = performance.now();
    const pin = () => {
      vp.scrollTop = vp.scrollHeight;
      lastScrollTopRef.current = vp.scrollTop;
      if (performance.now() - start < 500) raf = requestAnimationFrame(pin);
    };
    raf = requestAnimationFrame(pin);
    return () => cancelAnimationFrame(raf);
  }, [keyboardOpen, composerCollapsed]);

  return {
    viewportRef,
    belowViewportRef,
    messagesContentRef,
    atBottom,
    isCoarse,
    scrollToBottom,
    requestEarlierHistory,
  };
}

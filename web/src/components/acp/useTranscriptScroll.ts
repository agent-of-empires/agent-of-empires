import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

import { useIsCoarsePointer } from "../../hooks/useIsCoarsePointer";
import { useMobileKeyboard } from "../../hooks/useMobileKeyboard";
import { loadScrollState, restoredScrollTop, saveScrollState } from "../../lib/acpScrollState";
import { anchorIsStale, autoLoadDecision, isPinnedToBottom, scrollRestoreDelta } from "../../lib/historyScroll";
import { promptRepinDecision } from "../../lib/promptRepin";
import { repinOnResize } from "../../lib/repinOnResize";

/** Stick-to-bottom, earlier-history auto-load, and PWA-reopen scroll restore
 *  for the transcript viewport. These observers own bottom-following; the
 *  viewport primitive's own auto-scroll must stay disabled. */
export function useTranscriptScroll({
  sessionId,
  canLoadEarlierHistory,
  loadEarlierHistory,
  loadingEarlierHistory,
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
  // Last time we sampled at the bottom. iOS fires an interim scroll during a
  // keyboard resize that clears `wasAtBottomRef` but cannot clear this.
  const lastAtBottomAtRef = useRef(0);
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
    lastAtBottomAtRef.current = performance.now();
    setAtBottom(true);
    vp.scrollTo({ top: vp.scrollHeight, behavior });
  }, []);
  const scrollToBottom = useCallback(() => pinToBottom("smooth"), [pinToBottom]);

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

  useLayoutEffect(() => {
    const vp = viewportRef.current;
    const below = belowViewportRef.current;
    const content = messagesContentRef.current;
    if (!vp || !below) return;
    // On coarse pointers the browser fires "scroll" for programmatic and
    // resize-driven scrolls too, so the stick intent is only re-sampled during
    // a real touch/wheel gesture there.
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
    const sample = (force = false) => {
      if (force || !isCoarse || gestureActive) {
        const pinned = isPinnedToBottom(vp.scrollTop, vp.clientHeight, vp.scrollHeight);
        const prevStuck = wasAtBottomRef.current;
        wasAtBottomRef.current = pinned;
        if (pinned) lastAtBottomAtRef.current = performance.now();
        setAtBottom((prev) => (prev === pinned ? prev : pinned));
        // Gated on the restore having run: the forced mount sample reads a tall
        // transcript at scrollTop 0 as unpinned and would clobber the saved intent.
        if (pinned !== prevStuck && didRestoreScrollRef.current) {
          saveScrollState(sessionId, { stuck: pinned, top: vp.scrollTop });
        }
        // Momentum scrolling fires with no fresh touchmove; keep the gesture alive.
        if (gestureActive) scheduleGestureClear();
      }
      const decision = autoLoadDecision({
        scrollTop: vp.scrollTop,
        clientHeight: vp.clientHeight,
        scrollHeight: vp.scrollHeight,
        armed: autoLoadArmedRef.current,
        canLoadEarlier: canLoadEarlierRef.current,
        now: performance.now(),
        lastLoadAt: lastAutoLoadAtRef.current,
      });
      autoLoadArmedRef.current = decision.armed;
      if (decision.fire) requestEarlierHistory();
    };
    const onScroll = () => sample();
    sample(true);
    vp.addEventListener("scroll", onScroll, { passive: true });
    vp.addEventListener("wheel", markGesture, { passive: true });
    vp.addEventListener("touchmove", markGesture, { passive: true });
    // Pin on every visualViewport resize frame so the transcript tracks the
    // soft keyboard animation in lockstep.
    const vv = typeof window !== "undefined" ? window.visualViewport : null;
    const onVvResize = () => {
      if (wasAtBottomRef.current) vp.scrollTop = vp.scrollHeight;
    };
    vv?.addEventListener("resize", onVvResize);

    if (!didRestoreScrollRef.current) {
      didRestoreScrollRef.current = true;
      const saved = loadScrollState(sessionId);
      const stick = !saved || saved.stuck;
      wasAtBottomRef.current = stick;
      if (stick) lastAtBottomAtRef.current = performance.now();
      setAtBottom(stick);
      // Later passes catch content that lays out after paint; each rechecks the
      // current stick intent so an intervening user scroll wins.
      const applyStart = () => {
        const top = restoredScrollTop(saved, wasAtBottomRef.current, vp.scrollHeight, vp.clientHeight);
        if (top != null) vp.scrollTop = top;
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
    };
    const ro = repinOnResize({ target: below, readHeight: () => below.offsetHeight, wasAtBottom, repin });
    const vpRo = repinOnResize({ target: vp, readHeight: () => vp.clientHeight, wasAtBottom, repin });
    // Growth with a pending anchor came from older rows at the top: keep the
    // read position. Otherwise it grew at the bottom: follow if pinned.
    const contentRo = new ResizeObserver(() => {
      const anchor = pendingScrollAnchorRef.current;
      if (anchor != null) {
        const delta = scrollRestoreDelta(anchor, vp.scrollHeight, wasAtBottomRef.current);
        if (delta > 0) vp.scrollTop += delta;
        pendingScrollAnchorRef.current = null;
        return;
      }
      if (wasAtBottomRef.current) {
        vp.scrollTop = vp.scrollHeight;
      }
    });
    if (content) contentRo.observe(content);
    return () => {
      ro.disconnect();
      vpRo.disconnect();
      contentRo.disconnect();
      vp.removeEventListener("scroll", onScroll);
      vp.removeEventListener("wheel", markGesture);
      vp.removeEventListener("touchmove", markGesture);
      vv?.removeEventListener("resize", onVvResize);
      window.removeEventListener("pagehide", saveScroll);
      document.removeEventListener("visibilitychange", onVisibility);
      saveScroll();
      if (gestureClearTimer) window.clearTimeout(gestureClearTimer);
    };
  }, [requestEarlierHistory, isCoarse, sessionId]);

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

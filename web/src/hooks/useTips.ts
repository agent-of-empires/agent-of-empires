// Web dashboard tips controller. Owns everything the tip-of-the-day needs:
// fetches the web-surface tips from the shared catalog (GET /api/tips), tracks
// the modal open state and which tip it opens on, marks tips seen on view, and
// toggles the show-on-startup preference. Keeping this in the hook (not the App
// component) keeps the logic unit-testable and out of the App.tsx giant.
import { useCallback, useEffect, useState } from "react";
import { fetchTips, markTipSeen, setShowTips, type TipDto } from "../lib/api";

/** Inputs to the startup auto-pop decision. Kept as primitives so the decision
 *  is a pure function the App effect can call and tests can pin. */
export interface TipsAutoPopGate {
  /** GET /api/tips has resolved. */
  loaded: boolean;
  /** At least one eligible tip is unseen and tips are enabled. */
  hasUnseen: boolean;
  /** Whether the tour was already seen when the page loaded (not the live flag:
   *  finishing the tour this session must not then pop tips on top of it). */
  tourSeenAtLoad: boolean | null;
  /** The dashboard has settled and the theme welcome is resolved. */
  onboardingReady: boolean;
  /** The telemetry consent modal is up. */
  telemetryPending: boolean;
  /** The tour is currently running. */
  tourActive: boolean;
  /** An automated browser session (Playwright), where modals must not auto-pop. */
  automated: boolean;
}

/** Pure decision for the startup auto-pop, mirroring the tour's shouldAutoLaunch:
 *  only for a settled dashboard, only when a tip is unseen, only for users who
 *  already finished onboarding before this load, and never while another flow is
 *  up or in an automated session. */
export function shouldAutoPopTips(g: TipsAutoPopGate): boolean {
  return (
    g.loaded &&
    g.hasUnseen &&
    g.tourSeenAtLoad === true &&
    g.onboardingReady &&
    !g.telemetryPending &&
    !g.tourActive &&
    !g.automated
  );
}

export interface UseTipsResult {
  /** Whether tips show on startup (session.show_tips). */
  enabled: boolean;
  /** Web-eligible tips in catalog order, with seen state. */
  tips: TipDto[];
  /** True once GET /api/tips has resolved. */
  loaded: boolean;
  /** Whether any tip is unseen; gates the startup auto-pop. */
  hasUnseen: boolean;
  /** Whether the tip-of-the-day modal is open. */
  isOpen: boolean;
  /** Tip index the modal opens on (the first unseen when opened). */
  startIndex: number;
  /** Open the modal on the first unseen tip and mark that tip seen. */
  open: () => void;
  /** Close the modal. */
  close: () => void;
  /** Mark one tip seen locally and on the server (mark-seen-on-view). */
  markSeen: (id: string) => void;
  /** Set "Show tips on startup" locally and on the server. */
  setEnabled: (enabled: boolean) => void;
}

export function useTips(): UseTipsResult {
  const [enabled, setEnabledState] = useState(false);
  const [tips, setTips] = useState<TipDto[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [isOpen, setIsOpen] = useState(false);
  const [startIndex, setStartIndex] = useState(0);

  useEffect(() => {
    let active = true;
    fetchTips().then((resp) => {
      if (!active) return;
      if (resp) {
        setEnabledState(resp.enabled);
        setTips(resp.tips);
      }
      setLoaded(true);
    });
    return () => {
      active = false;
    };
  }, []);

  const markSeen = useCallback((id: string) => {
    // Optimistic: flip locally so the modal reflects it immediately, then
    // persist. A failed write is nonfatal; the server stays authoritative on
    // the next load.
    setTips((prev) => prev.map((t) => (t.id === id ? { ...t, seen: true } : t)));
    void markTipSeen(id);
  }, []);

  const setEnabled = useCallback((next: boolean) => {
    setEnabledState(next);
    void setShowTips(next);
  }, []);

  const firstUnseen = tips.findIndex((t) => !t.seen);

  // Open on the first unseen tip; capture the index before marking it seen,
  // since marking shifts firstUnseen and would otherwise skip that tip.
  const open = useCallback(() => {
    const idx = firstUnseen === -1 ? 0 : firstUnseen;
    setStartIndex(idx);
    setIsOpen(true);
    const tip = tips[idx];
    if (tip && !tip.seen) markSeen(tip.id);
  }, [tips, firstUnseen, markSeen]);

  const close = useCallback(() => setIsOpen(false), []);

  return {
    enabled,
    tips,
    loaded,
    hasUnseen: enabled && firstUnseen !== -1,
    isOpen,
    startIndex,
    open,
    close,
    markSeen,
    setEnabled,
  };
}

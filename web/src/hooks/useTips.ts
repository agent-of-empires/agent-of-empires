// Web dashboard tips controller. Fetches the web-surface tips from the shared
// catalog (GET /api/tips) and exposes what the tip-of-the-day modal needs: the
// list, whether tips show on startup, and the first unseen tip to open on. Marks
// tips seen on view and toggles the startup preference through the dedicated tips
// endpoints, so state stays shared with the TUI across devices.
import { useCallback, useEffect, useState } from "react";
import { fetchTips, markTipSeen, setShowTips, type TipDto } from "../lib/api";

export interface UseTipsResult {
  /** Whether tips show on startup (session.show_tips). */
  enabled: boolean;
  /** Web-eligible tips in catalog order, with seen state. */
  tips: TipDto[];
  /** True once GET /api/tips has resolved, so callers don't act on empty state. */
  loaded: boolean;
  /** Whether any tip is unseen; gates the startup auto-pop. */
  hasUnseen: boolean;
  /** Index of the first unseen tip, or 0 when all are seen. The modal opens
   *  here so new content leads. */
  firstUnseenIndex: number;
  /** Mark one tip seen locally and on the server (mark-seen-on-view). */
  markSeen: (id: string) => void;
  /** Set "Show tips on startup" locally and on the server. */
  setEnabled: (enabled: boolean) => void;
}

export function useTips(): UseTipsResult {
  const [enabled, setEnabledState] = useState(false);
  const [tips, setTips] = useState<TipDto[]>([]);
  const [loaded, setLoaded] = useState(false);

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
  const hasUnseen = enabled && firstUnseen !== -1;

  return {
    enabled,
    tips,
    loaded,
    hasUnseen,
    firstUnseenIndex: firstUnseen === -1 ? 0 : firstUnseen,
    markSeen,
    setEnabled,
  };
}

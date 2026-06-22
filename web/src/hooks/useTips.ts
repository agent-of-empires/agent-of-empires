// Web dashboard tips controller. Fetches the web-surface tips from the shared
// catalog (GET /api/tips), exposes the unseen count for the TopBar badge, and
// persists mark-seen-on-view and "don't show again" through the dedicated tips
// endpoints so state stays shared with the TUI across devices. Rotation tips
// are passive: the badge surfaces them, but nothing pops on its own, mirroring
// the TUI's rotation semantics.
import { useCallback, useEffect, useState } from "react";
import { disableTips, fetchTips, markTipSeen, type TipDto } from "../lib/api";

export interface UseTipsResult {
  /** Whether tips are enabled (session.show_tips). */
  enabled: boolean;
  /** Web-eligible tips in catalog order, with seen state. */
  tips: TipDto[];
  /** Count of unseen tips; drives the badge (0 hides it). */
  unseenCount: number;
  /** Mark one tip seen locally and on the server (mark-seen-on-view). */
  markSeen: (id: string) => void;
  /** Turn tips off; clears local state so the badge and panel hide at once. */
  disable: () => void;
}

export function useTips(): UseTipsResult {
  const [enabled, setEnabled] = useState(false);
  const [tips, setTips] = useState<TipDto[]>([]);

  useEffect(() => {
    let active = true;
    fetchTips().then((resp) => {
      if (!active || !resp) return;
      setEnabled(resp.enabled);
      setTips(resp.tips);
    });
    return () => {
      active = false;
    };
  }, []);

  const markSeen = useCallback((id: string) => {
    // Optimistic: flip locally so the panel and badge update immediately, then
    // persist. A failed write is nonfatal; the server stays authoritative on
    // the next load.
    setTips((prev) => prev.map((t) => (t.id === id ? { ...t, seen: true } : t)));
    void markTipSeen(id);
  }, []);

  const disable = useCallback(() => {
    setEnabled(false);
    void disableTips();
  }, []);

  const unseenCount = enabled ? tips.filter((t) => !t.seen).length : 0;

  return { enabled, tips, unseenCount, markSeen, disable };
}

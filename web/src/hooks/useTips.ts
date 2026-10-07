import { useCallback, useEffect, useState } from "react";
import { fetchTips, markTipSeen, setShowTips, type TipDto } from "../lib/api";

export interface TipsAutoPopGate {
  loaded: boolean;
  hasUnseen: boolean;
  tourSeenAtLoad: boolean | null;
  onboardingReady: boolean;
  telemetryPending: boolean;
  tourActive: boolean;
  automated: boolean;
}

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
  enabled: boolean;
  tips: TipDto[];
  loaded: boolean;
  hasUnseen: boolean;
  isOpen: boolean;
  startIndex: number;
  open: () => void;
  close: () => void;
  markSeen: (id: string) => void;
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
    setTips((prev) => prev.map((t) => (t.id === id ? { ...t, seen: true } : t)));
    void markTipSeen(id);
  }, []);

  const setEnabled = useCallback((next: boolean) => {
    setEnabledState(next);
    void setShowTips(next);
  }, []);

  const firstUnseen = tips.findIndex((t) => !t.seen);

  // Capture the index before marking seen, which shifts firstUnseen.
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

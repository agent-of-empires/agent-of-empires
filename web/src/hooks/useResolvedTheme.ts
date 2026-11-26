import { useEffect, useSyncExternalStore } from "react";
import { fetchCurrentTheme, fetchResolvedTheme } from "../lib/api";
import {
  getResolvedTheme,
  parseTheme,
  setResolvedTheme,
  subscribeTheme,
  THEME_STORAGE_KEY,
  type ResolvedTheme,
} from "../lib/theme";
import { listen } from "./domEvents";

export const THEME_PICKER_CHANGED_EVENT = "aoe:theme-picker-changed";

export interface ThemePickerChangedDetail {
  name?: string;
}

/** The active theme; re-renders on every change. */
export function useCurrentTheme(): ResolvedTheme | null {
  return useSyncExternalStore(subscribeTheme, getResolvedTheme);
}

/** Keeps the store in sync with the Daemon and other windows. Mount once, at the app root. */
export function useResolvedTheme(): ResolvedTheme | null {
  useEffect(() => {
    // Apply only responses newer than the last applied, so a slow fetch can't override a pick.
    let nextSeq = 0;
    let lastAppliedSeq = 0;
    let unmounted = false;
    const load = (name?: string) => {
      const seq = ++nextSeq;
      (name ? fetchResolvedTheme(name) : fetchCurrentTheme()).then((next) => {
        if (unmounted || !next || seq <= lastAppliedSeq) return;
        lastAppliedSeq = seq;
        setResolvedTheme(next);
      });
    };

    load();
    const onPick = (event: Event) => load((event as CustomEvent<ThemePickerChangedDetail>).detail?.name);
    // Another window of this browser picked a theme and cached it.
    const onStorage = (event: Event) => {
      const { key, newValue } = event as StorageEvent;
      const next = key === THEME_STORAGE_KEY ? parseTheme(newValue) : null;
      if (!next) return;
      // Newer than any fetch in flight, which would otherwise repaint and re-cache the old theme.
      lastAppliedSeq = ++nextSeq;
      setResolvedTheme(next, false);
    };
    // A pick on another device or in the TUI shows up when this window comes back.
    const onVisible = () => {
      if (document.visibilityState === "visible") load();
    };
    const stops = [
      listen(onPick, [window, THEME_PICKER_CHANGED_EVENT]),
      listen(onStorage, [window, "storage"]),
      listen(onVisible, [document, "visibilitychange"]),
    ];
    return () => {
      unmounted = true;
      stops.forEach((stop) => stop());
    };
  }, []);

  return useCurrentTheme();
}

export function dispatchThemePickerChanged(name?: string): void {
  window.dispatchEvent(
    new CustomEvent<ThemePickerChangedDetail>(THEME_PICKER_CHANGED_EVENT, {
      detail: { name },
    }),
  );
}

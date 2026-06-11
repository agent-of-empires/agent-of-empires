import { useEffect, useState } from "react";
import { fetchCurrentTheme, fetchResolvedTheme } from "../lib/api";
import { applyResolvedTheme, dispatchThemeChanged, readCachedResolvedTheme, type ResolvedTheme } from "../lib/theme";

/** Event name fired by the settings UI after the user picks a new
 *  theme. The hook listens for it and refetches /api/theme/current
 *  (or, if the event carries a `detail.name`, /api/themes/:name) so
 *  the dashboard repaints without a settings round-trip. */
export const THEME_PICKER_CHANGED_EVENT = "aoe:theme-picker-changed";

export interface ThemePickerChangedDetail {
  /** New theme name selected by the user. Optional: when omitted the
   *  hook refetches /api/theme/current. */
  name?: string;
}

/** Apply the user's selected theme on mount and on settings updates.
 *  Reads the cached payload from localStorage first to prevent FOUC,
 *  then fetches the authoritative payload and applies it. */
export function useResolvedTheme(): ResolvedTheme | null {
  const [theme, setTheme] = useState<ResolvedTheme | null>(() => readCachedResolvedTheme());

  useEffect(() => {
    // Monotonic sequence numbers tag every in-flight fetch. A fetch's
    // response is applied only if its sequence is greater than the
    // most recently applied one, so a slow mount fetch landing after
    // a faster user-initiated picker fetch can't overwrite the user's
    // pick. Unmount drops `unmounted` so late responses are no-ops.
    let nextSeq = 0;
    let lastAppliedSeq = 0;
    let unmounted = false;
    const apply = (next: ResolvedTheme | null, seq: number) => {
      if (unmounted || !next || seq <= lastAppliedSeq) return;
      lastAppliedSeq = seq;
      applyResolvedTheme(next);
      dispatchThemeChanged(next);
      setTheme(next);
    };

    const mountSeq = ++nextSeq;
    fetchCurrentTheme().then((next) => apply(next, mountSeq));

    const onChange = (event: Event) => {
      const detail = (event as CustomEvent<ThemePickerChangedDetail>).detail;
      const seq = ++nextSeq;
      const promise = detail?.name ? fetchResolvedTheme(detail.name) : fetchCurrentTheme();
      promise.then((next) => apply(next, seq));
    };
    window.addEventListener(THEME_PICKER_CHANGED_EVENT, onChange);
    return () => {
      unmounted = true;
      window.removeEventListener(THEME_PICKER_CHANGED_EVENT, onChange);
    };
  }, []);

  return theme;
}

/** Fire from the settings UI after the user picks a new theme. */
export function dispatchThemePickerChanged(name?: string): void {
  window.dispatchEvent(
    new CustomEvent<ThemePickerChangedDetail>(THEME_PICKER_CHANGED_EVENT, {
      detail: { name },
    }),
  );
}

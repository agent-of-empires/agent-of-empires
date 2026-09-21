import { useEffect, useState } from "react";
import { fetchCurrentTheme, fetchResolvedTheme } from "../lib/api";
import { applyResolvedTheme, dispatchThemeChanged, readCachedResolvedTheme, type ResolvedTheme } from "../lib/theme";
import { listen } from "./domEvents";

export const THEME_PICKER_CHANGED_EVENT = "aoe:theme-picker-changed";

export interface ThemePickerChangedDetail {
  name?: string;
}

export function useResolvedTheme(): ResolvedTheme | null {
  const [theme, setTheme] = useState<ResolvedTheme | null>(() => readCachedResolvedTheme());

  useEffect(() => {
    // Apply only responses newer than the last applied, so a slow mount fetch can't override a pick.
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
    const stop = listen(onChange, [window, THEME_PICKER_CHANGED_EVENT]);
    return () => {
      unmounted = true;
      stop();
    };
  }, []);

  return theme;
}

export function dispatchThemePickerChanged(name?: string): void {
  window.dispatchEvent(
    new CustomEvent<ThemePickerChangedDetail>(THEME_PICKER_CHANGED_EVENT, {
      detail: { name },
    }),
  );
}

import { useEffect, useState } from "react";

/** Tracks whether the viewport is at least Tailwind's `md` breakpoint
 *  (768px) wide. Reactive to `(min-width: 768px)` matchMedia changes,
 *  SSR-safe via a `typeof window` guard on the initial read.
 *
 *  Layout topology (side-by-side split vs single-pane picker) is driven
 *  by width so it stays aligned with the `md:` Tailwind classes the rest
 *  of the layout uses; pointer type (`useIsCoarsePointer` /
 *  `useMobileKeyboard`) governs touch affordances and keyboard padding,
 *  not which panes exist. Gating layout on pointer type instead would
 *  force a touch laptop into the mobile UI and squash a narrow desktop
 *  window into a cramped split. */
export function useIsWideViewport(): boolean {
  const [isWide, setIsWide] = useState(
    () =>
      typeof window !== "undefined" &&
      Boolean(window.matchMedia?.("(min-width: 768px)").matches),
  );
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mql = window.matchMedia("(min-width: 768px)");
    const onChange = () => setIsWide(mql.matches);
    mql.addEventListener?.("change", onChange);
    return () => mql.removeEventListener?.("change", onChange);
  }, []);
  return isWide;
}

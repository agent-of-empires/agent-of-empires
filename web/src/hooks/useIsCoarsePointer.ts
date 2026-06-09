import { useEffect, useState } from "react";

/** Tracks whether the primary pointer is coarse (touchscreen). Reactive
 *  to `(pointer: coarse)` matchMedia changes, SSR-safe via a `typeof
 *  window` guard on the initial read. Use when you need a coarse /
 *  fine distinction outside Tailwind's responsive class set, e.g. to
 *  pick mobile vs desktop defaults for component layout state.
 *
 *  `useMobileKeyboard` exposes an `isMobile` flag built from the same
 *  query; consumers that need only the coarse / fine bit (without the
 *  keyboard tracking) should prefer this hook to avoid the extra
 *  visualViewport listeners. */
export function useIsCoarsePointer(): boolean {
  const [isCoarse, setIsCoarse] = useState(
    () => typeof window !== "undefined" && Boolean(window.matchMedia?.("(pointer: coarse)").matches),
  );
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mql = window.matchMedia("(pointer: coarse)");
    const onChange = () => setIsCoarse(mql.matches);
    mql.addEventListener?.("change", onChange);
    return () => mql.removeEventListener?.("change", onChange);
  }, []);
  return isCoarse;
}

import { useEffect, useState } from "react";

/** Tracks a media query. `watchResize` also re-reads on resize, for browsers that skip the change event. */
export function useMediaQuery(query: string, watchResize = false): boolean {
  const [matches, setMatches] = useState(
    () => typeof window !== "undefined" && Boolean(window.matchMedia?.(query).matches),
  );
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mql = window.matchMedia(query);
    const onChange = () => setMatches(mql.matches);
    mql.addEventListener?.("change", onChange);
    if (watchResize) window.addEventListener("resize", onChange);
    return () => {
      mql.removeEventListener?.("change", onChange);
      if (watchResize) window.removeEventListener("resize", onChange);
    };
  }, [query, watchResize]);
  return matches;
}

import { useEffect, useRef, type DependencyList } from "react";

/** Holds the virtualized diff at a scroll fraction (null means the top) across
 *  its async row reflows until the user scrolls. Returns the live scroller and
 *  a flag callers set when they scroll programmatically. */
export function useDiffScrollHold(targetFraction: () => number | null, deps: DependencyList) {
  const wrapRef = useRef<HTMLDivElement | null>(null);
  const scrollerRef = useRef<HTMLElement | null>(null);
  const userScrolledRef = useRef(false);

  useEffect(() => {
    const scroller = wrapRef.current?.querySelector<HTMLElement>(".overflow-auto");
    const content = scroller?.firstElementChild;
    if (!scroller || !content) return;
    scrollerRef.current = scroller;
    userScrolledRef.current = false;
    const frac = targetFraction();
    const apply = () => {
      if (userScrolledRef.current) return;
      scroller.scrollTop = frac == null ? 0 : frac * (scroller.scrollHeight - scroller.clientHeight);
    };
    const markUser = () => {
      userScrolledRef.current = true;
    };
    const events = ["wheel", "pointerdown", "keydown"] as const;
    for (const ev of events) scroller.addEventListener(ev, markUser, ev === "keydown" ? undefined : { passive: true });
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(content);
    return () => {
      ro.disconnect();
      for (const ev of events) scroller.removeEventListener(ev, markUser);
      if (scrollerRef.current === scroller) scrollerRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  return { wrapRef, scrollerRef, userScrolledRef };
}

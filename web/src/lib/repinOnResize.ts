// Keep a bottom-pinned scroller pinned when it resizes; split out of StructuredView for testing.

export interface RepinOnResizeOptions {
  target: Element;
  /** Width-only and net-zero resizes also fire the observer but cannot move the bottom. */
  readHeight: () => number;
  /** Must be sampled at scroll time; by the time the observer fires, layout has already settled. */
  wasAtBottom: () => boolean;
  repin: () => void;
}

/** Re-pin on a real height change (shrinking strands the transcript short of the bottom) unless the user had scrolled away. The caller disconnects the returned observer. */
export function repinOnResize({ target, readHeight, wasAtBottom, repin }: RepinOnResizeOptions): ResizeObserver {
  let prevHeight = readHeight();
  const ro = new ResizeObserver(() => {
    const nextHeight = readHeight();
    if (nextHeight === prevHeight) return;
    prevHeight = nextHeight;
    if (wasAtBottom()) repin();
  });
  ro.observe(target);
  return ro;
}

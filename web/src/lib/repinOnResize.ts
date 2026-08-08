// Keeps a bottom-pinned scroller pinned when something resizes it. Split out
// of StructuredView so the observer wiring is unit-testable without mounting
// the assistant-ui runtime (the #1282 pattern).

export interface RepinOnResizeOptions {
  /** Element to observe. */
  target: Element;
  /** Height that matters for this target, read at subscribe time and again on
   *  every callback. `ResizeObserver` also fires for width-only changes and for
   *  a resize that nets to zero, neither of which can move the scroll bottom. */
  readHeight: () => number;
  /** Whether the scroller was pinned to the bottom *before* this resize. Must
   *  be sampled at scroll time: by the time the observer fires, layout has
   *  already settled at the new height, so reading pinned-ness now would report
   *  false for exactly the case worth catching. */
  wasAtBottom: () => boolean;
  /** Put the scroller back at the bottom. */
  repin: () => void;
}

/** Observe `target` and re-pin whenever its height actually changes.
 *
 *  Only shrinking needs this: growing is self-correcting because the browser
 *  clamps `scrollTop` to the new maximum, while shrinking silently leaves the
 *  transcript a few hundred pixels short of the bottom. The pin is skipped
 *  when the user had scrolled away, so a resize never yanks them back down.
 *
 *  Returns the observer; the caller disconnects it on cleanup. */
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

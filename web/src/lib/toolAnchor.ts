// Holds the reader's place across a tool card being remounted (a run folding
// into a group), which the browser's own scroll anchoring cannot follow.

export interface ToolAnchor {
  id: string;
  el: Element;
  /** Offset of the card's top edge from the viewport's top edge. */
  offset: number;
  /** The viewport's scroll position when sampled. */
  scrollTop: number;
}

const CARD = "[data-tool-id]";

/** Every tool card reaching into the viewport, top to bottom. A fold can swallow
 *  the first of them, so a later one has to be able to take over. */
export function sampleToolAnchors(viewport: HTMLElement): ToolAnchor[] {
  const { top, bottom } = viewport.getBoundingClientRect();
  const cards = viewport.querySelectorAll(CARD);
  // Cards do not nest and sit in document order, so a binary search finds the
  // first one below the viewport top without measuring every card above it.
  let lo = 0;
  let hi = cards.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (cards[mid]!.getBoundingClientRect().bottom <= top) lo = mid + 1;
    else hi = mid;
  }
  const anchors: ToolAnchor[] = [];
  for (let i = lo; i < cards.length; i++) {
    const el = cards[i]!;
    const rect = el.getBoundingClientRect();
    if (rect.top >= bottom) break;
    anchors.push({ id: el.getAttribute("data-tool-id")!, el, offset: rect.top - top, scrollTop: viewport.scrollTop });
  }
  return anchors;
}

/** Fresh anchors, unless one was folded away: a scroll event can fire in the
 *  frame a fold lands, ahead of the correction that still needs the old ones. */
export function resampleToolAnchors(viewport: HTMLElement, previous: readonly ToolAnchor[]): readonly ToolAnchor[] {
  return previous.every(({ el }) => el.isConnected) ? sampleToolAnchors(viewport) : previous;
}

/** Scroll delta that puts the reader's place back, from the first anchor that
 *  was remounted. A surviving card stops the search: ordinary layout shifts
 *  stay native. An anchor folded out of sight is skipped for the next one.
 *  Scrolling since the sample is the reader's own and stays. */
export function toolAnchorDelta(viewport: HTMLElement, anchors: readonly ToolAnchor[]): number {
  for (const anchor of anchors) {
    if (anchor.el.isConnected) return 0;
    const next = viewport.querySelector(`[data-tool-id="${CSS.escape(anchor.id)}"]`);
    if (next) {
      // A fold shrinks the content, and the browser clamps the scroll to the new
      // end; that move is not the reader's.
      const sampled = Math.min(anchor.scrollTop, viewport.scrollHeight - viewport.clientHeight);
      const expected = anchor.offset - (viewport.scrollTop - sampled);
      return next.getBoundingClientRect().top - viewport.getBoundingClientRect().top - expected;
    }
  }
  return 0;
}

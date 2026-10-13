/** Largest backlog a drag may queue: a full-screen drag, bounded so a flick cannot queue a storm. */
export const MAX_QUEUED_TOUCH_NOTCHES = 64;
/** Largest single release; the app drains every queued wheel report before one repaint. */
const NOTCH_BURST_MAX = 6;
/** Backlog each extra line in a burst is worth, so a slow drag moves one line at a time. */
const NOTCH_BURST_DIVISOR = 4;
/** Release gap when no frame acknowledges the last burst: one animation frame. */
const NOTCH_FALLBACK_GAP_MS = 16;
const DEBUG_RATE_WINDOW_MS = 2000;

/** Debug overlay timing: arrivals in the rate window and mean arrival-to-commit latency, mutated in place. */
export class FrameTimingProbe {
  private arrivals: number[] = [];
  private latencySum = 0;
  private latencyCount = 0;

  record(now: number, receivedAt: number | undefined) {
    this.arrivals.push(now);
    while (this.arrivals.length > 0 && now - this.arrivals[0]! > DEBUG_RATE_WINDOW_MS) this.arrivals.shift();
    if (receivedAt != null) {
      this.latencySum += now - receivedAt;
      this.latencyCount += 1;
    }
  }

  fps(): number {
    return (this.arrivals.length * 1000) / DEBUG_RATE_WINDOW_MS;
  }

  meanPaintMs(): number {
    return this.latencySum / Math.max(1, this.latencyCount);
  }
}

/** Paces forward-mode wheel notches to the remote app's redraws. The first notch goes out at once; later
 *  bursts, sized to the backlog, wait for a frame (the app's acknowledgement) or the fallback gap.
 *  Opposite-direction input drops the pending run. */
export class NotchPacer {
  private notches = 0;
  private send: ((up: boolean, count: number) => void) | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private awaiting = false;

  enqueue(notches: number, send: (up: boolean, count: number) => void) {
    if (this.notches !== 0 && Math.sign(this.notches) !== Math.sign(notches)) this.notches = 0;
    this.notches = Math.max(-MAX_QUEUED_TOUCH_NOTCHES, Math.min(MAX_QUEUED_TOUCH_NOTCHES, this.notches + notches));
    this.send = send;
    if (!this.awaiting) this.flush();
  }

  onFrame() {
    if (this.awaiting && this.notches !== 0) this.flush();
  }

  cancel() {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.notches = 0;
    this.awaiting = false;
  }

  private flush() {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.awaiting = false;
    if (this.notches === 0 || !this.send) return;
    const up = this.notches < 0;
    const burst = Math.min(NOTCH_BURST_MAX, Math.ceil(Math.abs(this.notches) / NOTCH_BURST_DIVISOR));
    this.send(up, burst);
    this.notches += up ? burst : -burst;
    if (this.notches !== 0) {
      this.awaiting = true;
      this.timer = setTimeout(() => this.flush(), NOTCH_FALLBACK_GAP_MS);
    }
  }
}

/** Mounted row blocks for virtualization: each block's leading pad (in lines) and row range, plus the trailing pad. */
export function mountedBlocks(
  visibleRowCount: number,
  spacerLines: number,
  view: { top: number; height: number },
  lineH: number,
) {
  let ranges = [{ start: 0, end: visibleRowCount }];
  if (view.height > 0 && lineH > 0) {
    // One viewport of overscan each side, plus the live tail, which a bottom scroll may need before React sees it.
    const overscan = Math.ceil(view.height / lineH);
    const clamp = (n: number) => Math.max(0, Math.min(visibleRowCount, n));
    const firstVisible = Math.floor(view.top / lineH) - spacerLines;
    const lastVisible = Math.ceil((view.top + view.height) / lineH) - spacerLines;
    ranges = [
      { start: clamp(firstVisible - overscan), end: clamp(lastVisible + overscan) },
      { start: Math.max(0, visibleRowCount - overscan * 2), end: visibleRowCount },
    ];
  }
  const merged: { start: number; end: number }[] = [];
  for (const range of ranges.filter((r) => r.end > r.start).sort((a, b) => a.start - b.start)) {
    const prev = merged[merged.length - 1];
    if (prev && range.start <= prev.end) prev.end = Math.max(prev.end, range.end);
    else merged.push({ ...range });
  }
  let nextStart = 0;
  const blocks = merged.map((range, i) => {
    const padLines = i === 0 ? spacerLines + range.start : range.start - nextStart;
    nextStart = range.end;
    return { padLines, ...range };
  });
  const bottomPadLines = blocks.length === 0 ? spacerLines + visibleRowCount : visibleRowCount - nextStart;
  return { blocks, bottomPadLines };
}

/** Rendered rows: each loaded line's raw row, each visual row's source line and wrap offset, and each line's
 *  first row. */
export interface RowLayout {
  keys: readonly string[];
  lineStartRow: number[];
  source: Array<{ line: number; wrap: number }>;
}

/** Lines compared to find the anchor again after the oldest scrollback is trimmed. */
const ANCHOR_CONTEXT_LINES = 8;

/** A scroll position as a pane line, its wrap row, and the fraction of a row above the scroller top. Pane
 *  lines count from the top of the scrollback and the spacer holds one row per line. `context` is the raw rows
 *  of the loaded line and those after it. */
export interface ScrollAnchor {
  line: number;
  wrap: number;
  fraction: number;
  context: readonly string[];
}

export function scrollAnchor(top: number, spacerLines: number, layout: RowLayout, lineH: number): ScrollAnchor | null {
  const abs = Math.floor(top / lineH);
  const fraction = top / lineH - abs;
  const row = abs - spacerLines;
  if (row < 0) return { line: abs, wrap: 0, fraction, context: [] };
  const src = layout.source[row];
  if (!src) return null;
  const context = layout.keys.slice(src.line, src.line + ANCHOR_CONTEXT_LINES);
  return { line: spacerLines + src.line, wrap: src.wrap, fraction, context };
}

/** Local index of the anchor's line: where its pane line now falls or, when trimming the oldest scrollback has
 *  renumbered every line, the nearest earlier index whose lines match its context. */
function anchorLine(anchor: ScrollAnchor, spacerLines: number, keys: readonly string[]): number {
  const expected = anchor.line - spacerLines;
  const { context } = anchor;
  if (context.length === 0) return expected;
  // A context cut off by the end only counts once no full match exists, so a repeat near the end loses.
  const matches = (at: number, cutOff: boolean) =>
    context.every((key, k) => (at + k < keys.length ? keys[at + k] === key : cutOff));
  for (const cutOff of [false, true]) {
    for (let at = Math.min(expected, keys.length - 1); at >= 0; at--) if (matches(at, cutOff)) return at;
  }
  return expected;
}

/** The scrollTop that puts `anchor` back where it was, or null when its line is not in this layout. */
export function anchoredScrollTop(
  anchor: ScrollAnchor,
  spacerLines: number,
  layout: RowLayout,
  lineH: number,
): number | null {
  const local = anchorLine(anchor, spacerLines, layout.keys);
  if (local < 0) return (anchor.line + anchor.fraction) * lineH;
  const first = layout.lineStartRow[local];
  if (first == null) return null;
  const rows = (layout.lineStartRow[local + 1] ?? layout.source.length) - first;
  return (spacerLines + first + Math.min(anchor.wrap, rows - 1) + anchor.fraction) * lineH;
}

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

/** Rendered rows: each loaded line's plain text, each visual row's source line and wrap offset, each line's
 *  first row, and the index of the first line on the live screen. */
export interface RowLayout {
  keys: readonly string[];
  lineStartRow: number[];
  source: Array<{ line: number; wrap: number }>;
  screenStart: number;
}

/** History lines compared to find the anchor again once its line number has changed. */
const ANCHOR_CONTEXT_LINES = 8;
/** Non-space characters the context must hold before its joined text can identify re-wrapped rows. */
const MIN_REWRAP_CHARS = 16;

/** A scroll position as a pane line, its wrap row, and the fraction of a row above the scroller top. Pane lines
 *  count from the top of the scrollback and the spacer holds one row per line; `fromEnd` counts back from the
 *  end instead, which a trim or a switch between the VT grid and tmux capture leaves unchanged. `context` is the
 *  plain text of history lines starting `probe` lines below the anchor's: its own when it is a loaded history
 *  line, the first loaded ones for a spacer line, or those just above the live screen for a screen line, which
 *  is repainted in place. The oldest boundary, or a view of nothing but spacer, has no context and keeps its
 *  scrollbar position: one asks for the oldest line, and in the other nothing was read. */
export interface ScrollAnchor {
  line: number;
  fromEnd: number;
  wrap: number;
  fraction: number;
  probe: number;
  context: readonly string[];
}

export function scrollAnchor(
  top: number,
  viewHeight: number,
  spacerLines: number,
  layout: RowLayout,
  lineH: number,
): ScrollAnchor | null {
  const abs = Math.floor(top / lineH);
  const fraction = top / lineH - abs;
  const row = abs - spacerLines;
  const src = row < 0 ? { line: row, wrap: 0 } : layout.source[row];
  if (!src) return null;
  const line = spacerLines + src.line;
  const first =
    src.line < layout.screenStart ? Math.max(0, src.line) : Math.max(0, layout.screenStart - ANCHOR_CONTEXT_LINES);
  const keepPosition = top <= 0 || spacerLines * lineH >= top + viewHeight;
  const context = keepPosition
    ? []
    : layout.keys.slice(first, Math.min(first + ANCHOR_CONTEXT_LINES, layout.screenStart));
  return {
    line,
    fromEnd: spacerLines + layout.keys.length - line,
    wrap: src.wrap,
    fraction,
    probe: first - src.line,
    context,
  };
}

/** Local index of the anchor's line: where its context is found, searching outward from predictions counted from
 *  either end, else where its joined text is found after a re-wrap, else its line number, which holds while
 *  output only appends. */
function anchorLine(anchor: ScrollAnchor, spacerLines: number, keys: readonly string[]): number {
  const fromStart = anchor.line - spacerLines;
  const { context, probe } = anchor;
  if (context.length === 0) return fromStart;
  // A context cut off by the end only counts once no full match exists, so a repeat near the end loses.
  const matches = (at: number, cutOff: boolean) =>
    at >= 0 && at < keys.length && context.every((key, k) => (at + k < keys.length ? keys[at + k] === key : cutOff));
  const predictions = [fromStart + probe, keys.length - anchor.fromEnd + probe];
  const reach = Math.max(...predictions.map((p) => Math.max(p, keys.length - 1 - p)));
  for (const cutOff of [false, true]) {
    for (let d = 0; d <= reach; d++) {
      for (const p of predictions) {
        if (matches(p - d, cutOff)) return p - d - probe;
        if (d > 0 && matches(p + d, cutOff)) return p + d - probe;
      }
    }
  }
  const row = rewrappedRow(context, keys, predictions);
  return row == null ? fromStart : row - probe;
}

/** The row holding the first character of `context` once a width change has re-wrapped the rows: a re-wrap moves
 *  soft row breaks but keeps every character, so the joined text without whitespace still matches. Of several
 *  copies, the one nearest a prediction wins. */
function rewrappedRow(context: readonly string[], keys: readonly string[], predictions: number[]): number | null {
  const strip = (key: string) => key.replace(/\s+/g, "");
  const needle = context.map(strip).join("");
  if (needle.length < MIN_REWRAP_CHARS) return null;
  const starts: number[] = [];
  let text = "";
  for (const key of keys) {
    starts.push(text.length);
    text += strip(key);
  }
  // The last row starting at or before `offset` holds that character; empty rows before it share its start.
  const rowAt = (offset: number) => {
    let lo = 0;
    let hi = starts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (starts[mid]! <= offset) lo = mid;
      else hi = mid - 1;
    }
    return lo;
  };
  let best: number | null = null;
  let bestDistance = Number.POSITIVE_INFINITY;
  for (let at = text.indexOf(needle); at >= 0; at = text.indexOf(needle, at + 1)) {
    const row = rowAt(at);
    const distance = Math.min(...predictions.map((p) => Math.abs(p - row)));
    if (distance < bestDistance) {
      best = row;
      bestDistance = distance;
    }
  }
  return best;
}

/** The scrollTop that puts `anchor` back where it was, or null when its line is not in this layout. */
export function anchoredScrollTop(
  anchor: ScrollAnchor,
  spacerLines: number,
  layout: RowLayout,
  lineH: number,
): number | null {
  const local = anchorLine(anchor, spacerLines, layout.keys);
  if (local < 0) return Math.max(0, spacerLines + local + anchor.fraction) * lineH;
  const first = layout.lineStartRow[local];
  if (first == null) return null;
  const rows = (layout.lineStartRow[local + 1] ?? layout.source.length) - first;
  return (spacerLines + first + Math.min(anchor.wrap, rows - 1) + anchor.fraction) * lineH;
}

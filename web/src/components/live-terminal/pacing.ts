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

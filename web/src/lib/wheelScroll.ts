// Desktop mouse-wheel → tmux wheel-event accumulator.
//
// tmux's copy-mode wheel binding scrolls 5 lines per SGR wheel event,
// so each emitted event is a 5-line quantum; the job here is deciding
// WHEN to emit so the quantization feels like a native terminal rather
// than a dead zone followed by jumps:
//
// - deltaMode normalization: Firefox wheel mice report DOM_DELTA_LINE
//   (deltaY=3 per notch), which read as raw pixels would need ~28
//   notches to cross a 5-line pixel step. Line deltas convert at one
//   notch = one event, which is what a hardware terminal sends tmux.
// - From rest, the first event fires after a single line of travel so
//   the response is immediate; sustained scrolling then runs at the
//   5-lines-of-pixels-per-event cadence, keeping content speed 1:1
//   with a trackpad.
// - The accumulator resets when the direction flips or the wheel goes
//   idle, so stale travel never delays the next gesture.

/** Lines tmux scrolls per SGR wheel event (its copy-mode default). */
export const LINES_PER_WHEEL_EVENT = 5;
/** A pause longer than this starts a new gesture (fresh dead-zone). */
const IDLE_RESET_MS = 250;

export interface WheelInput {
  deltaY: number;
  /** WheelEvent.deltaMode: 0 pixel, 1 line, 2 page. */
  deltaMode: number;
  timeStamp: number;
}

export class WheelAccumulator {
  private accum = 0;
  private lastTs = Number.NEGATIVE_INFINITY;
  private lastSign = 0;
  private streakActive = false;

  /**
   * Feed one wheel event; returns the signed number of tmux wheel
   * events to emit now (positive = down). `cellHeight` is the current
   * line height in px and `viewportRows` the terminal row count (for
   * page-mode deltas).
   */
  feed(input: WheelInput, cellHeight: number, viewportRows: number): number {
    const cellH = cellHeight > 0 ? cellHeight : 1;
    const px =
      input.deltaMode === 1
        ? input.deltaY * cellH
        : input.deltaMode === 2
          ? input.deltaY * cellH * Math.max(1, viewportRows)
          : input.deltaY;

    if (
      input.timeStamp - this.lastTs > IDLE_RESET_MS ||
      (px !== 0 && this.lastSign !== 0 && Math.sign(px) !== this.lastSign)
    ) {
      this.accum = 0;
      this.streakActive = false;
    }
    this.lastTs = input.timeStamp;
    if (px !== 0) this.lastSign = Math.sign(px);
    this.accum += px;

    // Line-mode notches map one notch to one event regardless of the
    // OS lines-per-notch setting (a hardware terminal sends tmux one
    // wheel report per notch), so the step derives from the event's own
    // delta; pixel devices pay 5 lines of travel per event so content
    // tracks the gesture 1:1.
    const sustainedStep = (input.deltaMode === 1 ? Math.max(1, Math.abs(input.deltaY)) : LINES_PER_WHEEL_EVENT) * cellH;

    if (!this.streakActive) {
      if (Math.abs(this.accum) < cellH) return 0;
      // First event of a gesture: emit exactly one and absorb the
      // travel so the sustained cadence starts cleanly from here.
      const dir = Math.sign(this.accum);
      this.accum = 0;
      this.streakActive = true;
      return dir;
    }

    const events = Math.trunc(this.accum / sustainedStep);
    if (events !== 0) this.accum -= events * sustainedStep;
    return events;
  }
}

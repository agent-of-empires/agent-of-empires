// Decide whether a tap on the structured-view transcript should focus the
// composer (and so bring up the soft keyboard on mobile, see #2243).
//
// Only fires on coarse pointers: desktop already auto-focuses the composer on
// mount, and a fine-pointer click into the transcript is usually a
// select-to-copy. A tap that lands on an interactive control (tool card, link,
// button, the "load earlier" affordance) must do its own thing rather than pop
// the keyboard, and a tap that ends a text selection is left alone.
//
// Kept pure so the guard is unit-testable without mounting the assistant-ui
// runtime; the caller does the (synchronous, iOS-gesture-safe) focus dispatch.

const INTERACTIVE_SELECTOR = 'button, a, input, textarea, select, [role="button"], [contenteditable]';

export function shouldFocusComposerOnThreadTap(opts: {
  isCoarse: boolean;
  target: EventTarget | null;
  hasSelection: boolean;
}): boolean {
  if (!opts.isCoarse) return false;
  if (opts.hasSelection) return false;
  const el = opts.target instanceof Element ? opts.target : null;
  if (el?.closest(INTERACTIVE_SELECTOR)) return false;
  return true;
}

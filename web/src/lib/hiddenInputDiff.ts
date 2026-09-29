// Pure state diffing for the hidden terminal inputs. The textarea holds the sentinel plus the text the pane is
// believed to hold from it; every edit, whether a keystroke, IME rewrite, dictation hypothesis, or word delete, is
// sent as DELs back to the common prefix and then the new suffix.

/**
 * Invisible text that is never sent. iOS only autorepeats Backspace and issues word deletes while the field has
 * text, so the field is never empty; deleting into the sentinel sends DELs for pane text left of the tracked
 * region, and the sentinel is refilled. Line breaks rather than letters or spaces: text after a line break reads as
 * a fresh line, so dictation adds no leading space, iOS's double space period shortcut has no preceding word, and a
 * word delete that reaches it removes one line break (one DEL) instead of joining the sentinel to the typed word.
 */
export const HIDDEN_INPUT_SENTINEL = "\n\n\n";

export interface HiddenInputStep {
  /** Code points removed after the common prefix: one DEL each. */
  deleted: number;
  inserted: string;
  /** Sentinel text to restore at the start of the value; the next baseline is `refill + value`. */
  refill: string;
}

const isHighSurrogate = (code: number) => code >= 0xd800 && code <= 0xdbff;

export function stepHiddenInput(baseline: string, value: string): HiddenInputStep {
  let prefix = 0;
  const max = Math.min(baseline.length, value.length);
  while (prefix < max && baseline.charCodeAt(prefix) === value.charCodeAt(prefix)) prefix++;
  // Never split a surrogate pair: a changed emoji is deleted and retyped whole.
  if (prefix > 0 && isHighSurrogate(baseline.charCodeAt(prefix - 1))) prefix--;
  let kept = 0;
  while (kept < HIDDEN_INPUT_SENTINEL.length && value[kept] === HIDDEN_INPUT_SENTINEL[kept]) kept++;
  return {
    deleted: Array.from(baseline.slice(prefix)).length,
    inserted: value.slice(prefix),
    refill: HIDDEN_INPUT_SENTINEL.slice(kept),
  };
}

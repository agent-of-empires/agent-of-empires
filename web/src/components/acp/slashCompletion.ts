// Caret-relative text replacement for the composer's `/` command picker
// (#3418). Pure string math, no DOM: the caller owns writing the result
// back to the textarea.
//
// assistant-ui detects the trigger relative to the caret, but our old
// insert appended the command to the end of the buffer, so a command
// picked mid-message landed in the wrong place. Worse, the append also
// left assistant-ui's `cursorPosition` state stale, which kept the
// popover open and let a second `selectItem` strip the wrong range.
// Replacing the caret's own `/token` fixes both.

/** Matches assistant-ui's `WHITESPACE_RE` in `detectTrigger`, so a token
 *  boundary here is a token boundary there. Covers `\n`, so a multi-line
 *  composer resolves per line without extra handling. */
const WHITESPACE = /\s/u;

export interface SlashReplacement {
  /** The full new composer value. */
  text: string;
  /** Where the caret goes, always one past the whitespace that follows
   *  the inserted command. */
  cursor: number;
}

/** The `/token` the caret sits in, or null when there is none.
 *
 *  The backward half mirrors `detectTrigger`: scan left from the caret,
 *  give up at whitespace, and accept a `/` only at the start of the
 *  buffer or after whitespace (so `foo/bar` and `https://x` are not
 *  triggers). The forward half is ours: assistant-ui's range stops at
 *  the caret, but a command completion should consume the whole token,
 *  otherwise picking with the caret inside `/addrXYZ` would leave a
 *  stray `XYZ` behind. */
export function findSlashTokenRange(value: string, caret: number): { start: number; end: number } | null {
  const upToCaret = value.slice(0, caret);
  let start = -1;
  for (let i = upToCaret.length - 1; i >= 0; i--) {
    if (WHITESPACE.test(upToCaret[i]!)) return null;
    if (upToCaret[i] !== "/") continue;
    if (i > 0 && !WHITESPACE.test(upToCaret[i - 1]!)) continue;
    start = i;
    break;
  }
  if (start < 0) return null;

  let end = caret;
  while (end < value.length && !WHITESPACE.test(value[end]!)) end++;
  return { start, end };
}

/** Build the composer value that results from picking `commandId`.
 *
 *  When the caret sits in a `/token`, that token is replaced. Otherwise
 *  the command is inserted at the caret (replacing any selection), which
 *  is the toolbar-button path where the caret may have moved away from
 *  the trigger.
 *
 *  A whitespace boundary always follows the command, reusing the one
 *  already there when possible, and the caret lands past it. Without
 *  that boundary `detectTrigger`'s backward scan stays inside the
 *  command we just wrote, the popover re-opens, and it eats the next
 *  Enter instead of sending (#1512). */
export function replaceSlashCommand(
  value: string,
  selectionStart: number,
  selectionEnd: number,
  commandId: string,
): SlashReplacement {
  const command = `/${commandId}`;
  const token = findSlashTokenRange(value, selectionStart);

  const before = value.slice(0, token ? token.start : selectionStart);
  const after = value.slice(token ? token.end : Math.max(selectionEnd, selectionStart));
  // Only the insert-at-caret path can land mid-word; a replaced token
  // already had a valid boundary in front of it.
  const lead = !token && before.length > 0 && !WHITESPACE.test(before[before.length - 1]!) ? " " : "";
  const trail = after.length > 0 && WHITESPACE.test(after[0]!) ? "" : " ";

  return {
    text: before + lead + command + trail + after,
    cursor: before.length + lead.length + command.length + 1,
  };
}

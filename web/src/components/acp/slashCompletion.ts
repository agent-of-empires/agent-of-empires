// Caret-relative replacement of the `/token` for the composer command picker. Pure string math.

/** Same as assistant-ui's `detectTrigger` whitespace, so token boundaries agree. */
const WHITESPACE = /\s/u;

export interface SlashReplacement {
  text: string;
  /** One past the whitespace following the command. */
  cursor: number;
}

/** The `/token` around the caret, or null. The backward scan mirrors `detectTrigger`
 *  (a `/` counts only at the start or after whitespace); the forward scan consumes
 *  the rest of the token so no stray suffix is left behind. */
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

/** Replace the caret's `/token` with `commandId`, or insert at the caret when there is
 *  none. A whitespace boundary always follows, or trigger detection re-opens the
 *  popover on the command just written and it swallows the next Enter. */
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
  const lead = !token && before.length > 0 && !WHITESPACE.test(before[before.length - 1]!) ? " " : "";
  const trail = after.length > 0 && WHITESPACE.test(after[0]!) ? "" : " ";

  return {
    text: before + lead + command + trail + after,
    cursor: before.length + lead.length + command.length + 1,
  };
}

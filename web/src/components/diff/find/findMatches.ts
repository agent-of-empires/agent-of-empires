/**
 * In-diff find that searches a supplied set of lines (the diff *model*), not
 * the rendered DOM.
 *
 * The diff is rendered with a virtualized renderer (`@pierre/diffs`), so
 * off-screen lines are not in the DOM and the browser's native Cmd+F can't
 * reach them. Searching the model lets find/next/prev jump to a match anywhere
 * in the diff; the caller then scrolls the renderer to it.
 *
 * Callers pass exactly the lines that should be searchable; for the MVP, the
 * changed (added/deleted) lines of the diff (see `changedLines`), so find only
 * matches content that's actually part of the change, not the whole file.
 */

export type FindSide = "old" | "new";

/** A line that find is allowed to match against. */
export interface SearchableLine {
  side: FindSide;
  /** 1-based line number within that side. */
  lineNumber: number;
  text: string;
}

export interface FindMatch {
  side: FindSide;
  /** 1-based line number within that side. */
  lineNumber: number;
  /** 0-based start char offset within the line. */
  startCol: number;
  /** Exclusive end char offset within the line. */
  endCol: number;
  /** Global ordering index across all returned matches. */
  index: number;
}

export interface FindOptions {
  caseSensitive?: boolean;
  regex?: boolean;
}

/**
 * Find every non-overlapping occurrence of `query` across `lines`, preserving
 * the order of `lines` (callers pass them in rendered/diff order so next/prev
 * steps top-to-bottom).
 *
 * Returns an empty array for an empty query. Throws `SyntaxError` when `regex`
 * is set and `query` is not a valid regular expression, so the caller can
 * surface an "invalid pattern" state.
 */
export function findMatches(lines: SearchableLine[], query: string, opts: FindOptions = {}): FindMatch[] {
  if (query.length === 0) return [];

  const matcher = opts.regex
    ? regexMatcher(query, opts.caseSensitive ?? false)
    : literalMatcher(query, opts.caseSensitive ?? false);

  const matches: FindMatch[] = [];
  let index = 0;
  for (const line of lines) {
    for (const [startCol, endCol] of matcher(line.text)) {
      matches.push({
        side: line.side,
        lineNumber: line.lineNumber,
        startCol,
        endCol,
        index: index++,
      });
    }
  }
  return matches;
}

type LineMatcher = (line: string) => Array<[number, number]>;

function literalMatcher(query: string, caseSensitive: boolean): LineMatcher {
  const needle = caseSensitive ? query : query.toLowerCase();
  return (line) => {
    const hay = caseSensitive ? line : line.toLowerCase();
    const out: Array<[number, number]> = [];
    let from = 0;
    for (;;) {
      const at = hay.indexOf(needle, from);
      if (at === -1) break;
      out.push([at, at + needle.length]);
      from = at + needle.length;
    }
    return out;
  };
}

function regexMatcher(query: string, caseSensitive: boolean): LineMatcher {
  const flags = caseSensitive ? "g" : "gi";
  const re = new RegExp(query, flags);
  return (line) => {
    const out: Array<[number, number]> = [];
    re.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = re.exec(line)) !== null) {
      const start = m.index;
      const end = start + m[0].length;
      out.push([start, end]);
      // Guard against zero-width matches (e.g. `a*`) looping forever.
      re.lastIndex = m[0].length === 0 ? re.lastIndex + 1 : end;
    }
    return out;
  };
}

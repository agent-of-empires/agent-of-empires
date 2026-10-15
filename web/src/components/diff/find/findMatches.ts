// Find over the diff model: virtualized rows are not in the DOM, so native find cannot reach them.

export type FindSide = "old" | "new";

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
  index: number;
}

export interface FindOptions {
  caseSensitive?: boolean;
  regex?: boolean;
}

/** Non-overlapping matches in line order; throws `SyntaxError` for an invalid regex. */
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

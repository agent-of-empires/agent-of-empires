// Step-wise reveal for long prompts and reports, cut on line and paragraph boundaries.

export const STEP_LINES = 40;
export const PREVIEW_LINES = 4;

export interface Revealed {
  text: string;
  /** Lines still hidden. */
  remaining: number;
}

/** Closes a code fence a cut left open, with the marker that opened it. */
function closeFence(text: string): string {
  let open: string | null = null;
  for (const line of text.split("\n")) {
    const marker = /^\s*(`{3,}|~{3,})/.exec(line)?.[1];
    if (!marker) continue;
    if (!open) open = marker;
    else if (marker[0] === open[0] && marker.length >= open.length && /^\s*[`~]+\s*$/.test(line)) open = null;
  }
  return open ? `${text}\n${open}` : text;
}

/** The first `limit` lines of `text`. */
export function revealLines(text: string, limit: number): Revealed {
  const lines = text.split("\n");
  if (lines.length <= limit) return { text, remaining: 0 };
  return { text: closeFence(lines.slice(0, limit).join("\n")), remaining: lines.length - limit };
}

/** Whole paragraphs up to about `limit` lines; a first paragraph longer than that is cut by line. */
export function previewParagraphs(text: string, limit = PREVIEW_LINES): Revealed {
  const total = text.split("\n").length;
  if (total <= limit) return { text, remaining: 0 };
  const paragraphs = text.split(/\n[ \t]*\n/);
  let shown = paragraphs[0]!;
  if (shown.split("\n").length > limit) return revealLines(text, limit);
  for (const p of paragraphs.slice(1)) {
    const next = `${shown}\n\n${p}`;
    if (next.split("\n").length > limit) break;
    shown = next;
  }
  const remaining = total - shown.split("\n").length;
  return remaining > 0 ? { text: closeFence(shown), remaining } : { text, remaining: 0 };
}

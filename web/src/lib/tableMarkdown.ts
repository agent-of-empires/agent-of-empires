// Recovers a rendered table's markdown from the text it was parsed from.

/** The slice of a hast element that react-markdown passes to a `table` component. */
export interface TableNode {
  tagName?: string;
  position?: { start: { offset?: number }; end: { offset?: number } };
  children?: TableNode[];
}

// Blockquote markers and list indentation in front of a row.
const CONTAINER_PREFIX = /^(?:[ \t]*>)*[ \t]*/;
const LINE_ENDED = /^[ \t]*\r?\n/;
const LINE_ENDED_OR_EOF = /^[ \t]*(?:\r?\n|$)/;

interface Span {
  start: number;
  end: number;
}

function spanOf(node: TableNode): Span | null {
  const start = node.position?.start.offset;
  const end = node.position?.end.offset;
  return start === undefined || end === undefined ? null : { start, end };
}

function rowsOf(node: TableNode): TableNode[] {
  return (node.children ?? []).flatMap((child) => (child.tagName === "tr" ? [child] : rowsOf(child)));
}

/**
 * The table as standalone GFM, without blockquote or list prefixes.
 * `text` is the full source received so far; the node may come from a shorter
 * prefix of it. A last line that is still being written yields null: while the
 * message streams that is any last line without a line ending, once `complete`
 * the end of the text ends it too.
 */
export function tableSource(text: string, table: TableNode, complete: boolean): string | null {
  const tableSpan = spanOf(table);
  const rowSpans = rowsOf(table).map(spanOf);
  const header = rowSpans[0];
  if (!tableSpan || !header || rowSpans.some((span) => !span)) return null;
  if (!(complete ? LINE_ENDED_OR_EOF : LINE_ENDED).test(text.slice(tableSpan.end))) return null;

  const lines = rowSpans.map((span) => text.slice(span!.start, span!.end));
  // The delimiter row is no node of its own; it is the line between header and first body row.
  const gapEnd = rowSpans[1]?.start ?? tableSpan.end;
  const delimiter = text.slice(header.end, gapEnd).split("\n")[1]?.replace(/\r$/, "");
  if (!delimiter) return null;
  lines.splice(1, 0, delimiter.replace(CONTAINER_PREFIX, ""));
  return lines.join("\n");
}

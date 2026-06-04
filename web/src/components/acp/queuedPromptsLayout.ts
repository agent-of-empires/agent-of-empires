// Pure helpers for the structured view's QueuedPromptsStrip. Extracted from
// `StructuredView.tsx` so the layout decisions (visible count, "Show N
// more" label, per-row clamp threshold) can be unit-tested without
// mounting the strip + assistant-ui runtime. See #1232.

/** Per-row clamp heuristic. Triggers on either multi-line content
 *  (3+ lines, i.e. 2+ newlines, since shorter multi-line prompts fit
 *  naturally inside line-clamp-3 without truncating) or a single
 *  very-long line that would wrap past three rendered lines at the
 *  strip's typical width. False positives just show an unnecessary
 *  "…" affordance, which is cheap. */
export function isQueuedPromptLong(text: string): boolean {
  const lineCount = text.split("\n").length;
  return lineCount >= 3 || text.length > 160;
}

export interface QueuedStripLayout {
  /** Default visible row count for this viewport. */
  visibleDefault: number;
  /** True when collapse is active (queue exceeds default AND not user-expanded). */
  collapsed: boolean;
  /** How many rows render in the strip right now. */
  visibleCount: number;
  /** How many rows are hidden behind the toggle (0 when not collapsed). */
  hiddenCount: number;
  /** Label for the toggle button, or null when no toggle should render. */
  toggleLabel: "Show less" | string | null;
}

/** Decide how many queued-prompt rows the strip should show, and the
 *  toggle-button copy. Mobile gets a tighter default since a single
 *  multi-line prompt already dominates the small-viewport composer
 *  area. */
export function queuedStripLayout(args: {
  queuedCount: number;
  isMobile: boolean;
  expanded: boolean;
}): QueuedStripLayout {
  const { queuedCount, isMobile, expanded } = args;
  const visibleDefault = isMobile ? 1 : 2;
  const overflows = queuedCount > visibleDefault;
  const collapsed = overflows && !expanded;
  const visibleCount = collapsed ? visibleDefault : queuedCount;
  const hiddenCount = collapsed ? queuedCount - visibleDefault : 0;
  const toggleLabel = !overflows
    ? null
    : expanded
      ? "Show less"
      : `Show ${hiddenCount} more`;
  return { visibleDefault, collapsed, visibleCount, hiddenCount, toggleLabel };
}

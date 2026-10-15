/** Clamp rows with 3+ lines or a line long enough to wrap past three. */
export function isQueuedPromptLong(text: string): boolean {
  return text.split("\n").length >= 3 || text.length > 160;
}

export interface QueuedStripLayout {
  visibleDefault: number;
  /** Queue exceeds the default and the user has not expanded it. */
  collapsed: boolean;
  visibleCount: number;
  hiddenCount: number;
  toggleLabel: "Show less" | string | null;
}

/** Visible queued rows and toggle copy; mobile shows one row by default, desktop two. */
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
  const toggleLabel = !overflows ? null : expanded ? "Show less" : `Show ${hiddenCount} more`;
  return { visibleDefault, collapsed, visibleCount, hiddenCount, toggleLabel };
}

// Pure navigation logic for the composer's ArrowUp/ArrowDown queue recall
// (#2147). Kept out of the component so the branching (entry, step, stop
// at oldest, restore-draft past newest, drained-entry exit) is unit
// testable without mounting the assistant-ui composer. The component owns
// only the side effects (setText, focus, banner state).

import type { QueuedPrompt } from "../../lib/acpTypes";

/** Active browse cursor: the queued-prompt id currently loaded into the
 *  composer plus the draft that was there before browsing began. */
export interface RecallCursor {
  id: string;
  stashedDraft: string;
}

/** What the composer should do in response to a recall step.
 *  - `load`: set the cursor and load `text` for editing.
 *  - `restore`: load `text` (the stashed draft) and exit browse.
 *  - `exit`: end browse, leave the composer text as-is.
 *  - `none`: no-op (empty queue entry attempt, or already at the oldest). */
export type RecallNav =
  | { kind: "load"; cursor: RecallCursor; text: string }
  | { kind: "restore"; text: string }
  | { kind: "exit" }
  | { kind: "none" };

/** Resolve the next recall step. Anchored on the stable queued-prompt id
 *  so a background drain never targets the wrong row. `direction` is
 *  "older" (ArrowUp) or "newer" (ArrowDown); `currentDraft` is the
 *  composer text to stash when first entering recall. */
export function nextRecallTarget(
  queue: QueuedPrompt[],
  cursor: RecallCursor | null,
  direction: "older" | "newer",
  currentDraft: string,
): RecallNav {
  if (direction === "older") {
    if (queue.length === 0) return { kind: "exit" };
    if (!cursor) {
      const target = queue[queue.length - 1];
      if (!target) return { kind: "none" };
      return { kind: "load", cursor: { id: target.id, stashedDraft: currentDraft }, text: target.text };
    }
    const idx = queue.findIndex((p) => p.id === cursor.id);
    if (idx === -1) return { kind: "exit" };
    if (idx > 0) {
      const target = queue[idx - 1];
      if (!target) return { kind: "none" };
      return { kind: "load", cursor: { ...cursor, id: target.id }, text: target.text };
    }
    return { kind: "none" }; // oldest entry: no wrap
  }
  // "newer"
  if (!cursor) return { kind: "none" };
  const idx = queue.findIndex((p) => p.id === cursor.id);
  if (idx === -1) return { kind: "exit" };
  if (idx + 1 < queue.length) {
    const target = queue[idx + 1];
    if (!target) return { kind: "none" };
    return { kind: "load", cursor: { ...cursor, id: target.id }, text: target.text };
  }
  return { kind: "restore", text: cursor.stashedDraft };
}

/** Banner position for the active cursor, or null when the cursor is
 *  absent or its entry has drained. Position counts from the newest entry
 *  (1 = newest), matching the ArrowUp-from-newest browse order. */
export function recallBannerInfo(
  queue: QueuedPrompt[],
  cursor: RecallCursor | null,
): { pos: number; total: number } | null {
  if (!cursor) return null;
  const idx = queue.findIndex((p) => p.id === cursor.id);
  if (idx === -1) return null;
  return { pos: queue.length - idx, total: queue.length };
}

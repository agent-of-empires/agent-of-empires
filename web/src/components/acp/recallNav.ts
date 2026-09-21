// Composer ArrowUp/ArrowDown recall through the prompt queue, as pure navigation.

import type { QueuedPrompt } from "../../lib/acpTypes";

/** The queued prompt loaded for editing, plus the draft it displaced. */
export interface RecallCursor {
  id: string;
  stashedDraft: string;
}

/** `load` a queued prompt, `restore` the stashed draft and exit, `exit` leaving the text, or do nothing. */
export type RecallNav =
  | { kind: "load"; cursor: RecallCursor; text: string }
  | { kind: "restore"; text: string }
  | { kind: "exit" }
  | { kind: "none" };

/** Anchored on the queued-prompt id, so a background drain never retargets another row. */
export function nextRecallTarget(
  queue: QueuedPrompt[],
  cursor: RecallCursor | null,
  direction: "older" | "newer",
  currentDraft: string,
): RecallNav {
  if (direction === "older" && queue.length === 0) return { kind: "exit" };
  if (!cursor) {
    if (direction === "newer") return { kind: "none" };
    const target = queue[queue.length - 1]!;
    return { kind: "load", cursor: { id: target.id, stashedDraft: currentDraft }, text: target.text };
  }
  const idx = queue.findIndex((p) => p.id === cursor.id);
  if (idx === -1) return { kind: "exit" };
  const target = queue[direction === "older" ? idx - 1 : idx + 1];
  if (target) return { kind: "load", cursor: { ...cursor, id: target.id }, text: target.text };
  // No wrap past the oldest; past the newest the draft comes back.
  return direction === "older" ? { kind: "none" } : { kind: "restore", text: cursor.stashedDraft };
}

/** Banner position counted from the newest entry, or null when not browsing a live entry. */
export function recallBannerInfo(
  queue: QueuedPrompt[],
  cursor: RecallCursor | null,
): { pos: number; total: number } | null {
  if (!cursor) return null;
  const idx = queue.findIndex((p) => p.id === cursor.id);
  if (idx === -1) return null;
  return { pos: queue.length - idx, total: queue.length };
}

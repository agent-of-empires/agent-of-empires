// Where the reader was in each live terminal, so opening another session and
// coming back lands on the same lines instead of the live tail (the pane is
// remounted on every switch unless it is a kept-alive persistent terminal).
// In memory only: a reload starts every session at the live tail.
//
// Stored as the absolute row offset from the top of tmux history. History
// grows at the bottom, so new output that arrived while the reader was away
// does not shift the lines they were reading.

const topRowByKey = new Map<string, number>();

export function rememberReadingRow(key: string, topRow: number): void {
  if (Number.isFinite(topRow) && topRow >= 0) topRowByKey.set(key, topRow);
}

export function forgetReadingRow(key: string): void {
  topRowByKey.delete(key);
}

export function recallReadingRow(key: string): number | null {
  return topRowByKey.get(key) ?? null;
}

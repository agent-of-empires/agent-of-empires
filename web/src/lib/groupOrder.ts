// The Custom order of Group headers is each profile's `groups.json` order. One `POST /api/reorder` arranges a
// Group's siblings as listed and then moves it one slot.

import { moveId } from "./sidebarReorder";
import { compareNames } from "./sidebarSort";

export type GroupOrder = Record<string, string[]>;

export const parentPath = (path: string) => {
  const at = path.lastIndexOf("/");
  return at < 0 ? "" : path.slice(0, at);
};

/** One order for headers that merge every profile's Group of a path: profiles by name, each path where first seen. */
export function mergeGroupOrder(byProfile: GroupOrder): string[] {
  const names = Object.keys(byProfile).sort();
  return [...new Set(names.flatMap((name) => byProfile[name] ?? []))];
}

/**
 * Orders Group paths as the TUI's tree under Custom: a Group before its descendants, siblings in `order`, and
 * siblings it lacks after the listed ones, by name.
 */
export function groupPathComparator(order: readonly string[]): (a: string, b: string) => number {
  const rank = new Map(order.map((path, i) => [path, i] as const));
  return (a, b) => {
    const left = a.split("/");
    const right = b.split("/");
    for (let i = 0; i < Math.min(left.length, right.length); i++) {
      if (left[i] === right[i]) continue;
      const l = rank.get(left.slice(0, i + 1).join("/")) ?? Infinity;
      const r = rank.get(right.slice(0, i + 1).join("/")) ?? Infinity;
      if (l !== r) return l - r;
      // Paths are case-sensitive, so names equal but for case still need an order.
      return compareNames(left[i]!, right[i]!) || (left[i]! < right[i]! ? -1 : 1);
    }
    return left.length - right.length;
  };
}

export interface GroupMove {
  direction: "up" | "down";
  /** The siblings as the Daemon arranges them before the one-slot move. */
  siblings: string[];
  /** The profile's order once it lands. */
  order: string[];
}

/** `order` followed by the paths of `shown` it lacks, by name, where the sidebar shows them. */
function withShown(order: readonly string[], shown: readonly string[]): string[] {
  const listed = new Set(order);
  return [...order, ...[...new Set(shown)].filter((p) => !listed.has(p)).sort(compareNames)];
}

/** `full` with the children of `parent` replaced, slot for slot, by `siblings`. */
function placeSiblings(full: readonly string[], parent: string, siblings: readonly string[]): string[] {
  let next = 0;
  return full.map((p) => (parentPath(p) === parent ? siblings[next++]! : p));
}

/** The move that leaves `active` at its index in `moved`, coming from the neighbouring slot. */
function moveInto(moved: readonly string[], active: string, direction: GroupMove["direction"]) {
  const to = moved.indexOf(active);
  const from = direction === "up" ? to + 1 : to - 1;
  const siblings = [...moved];
  [siblings[to], siblings[from]] = [siblings[from]!, siblings[to]!];
  return siblings;
}

/**
 * The move that puts `active` next to its sibling `over`, after it when the drag went down. `shown` are the paths
 * on screen; one `order` lacks is placed after the listed ones by name, where the sidebar shows it.
 */
export function groupMove(
  order: readonly string[],
  shown: readonly string[],
  active: string,
  over: string,
  after: boolean,
): GroupMove | null {
  const parent = parentPath(active);
  if (active === over || parentPath(over) !== parent) return null;
  const full = withShown(order, shown);
  const siblings = full.filter((p) => parentPath(p) === parent);
  if (!siblings.includes(active) || !siblings.includes(over)) return null;
  const moved = moveId(siblings, active, over, after);
  const from = siblings.indexOf(active);
  const to = moved.indexOf(active);
  if (from === to) return null;
  const direction = to < from ? "up" : "down";
  return { direction, siblings: moveInto(moved, active, direction), order: placeSiblings(full, parent, moved) };
}

/**
 * The moves, one per sibling set, that put every Group of `order` and `shown` among its siblings in `compare`
 * order. Each move's `order` includes the moves before it.
 */
export function arrangeGroups(
  order: readonly string[],
  shown: readonly string[],
  compare: (a: string, b: string) => number,
): { path: string; move: GroupMove }[] {
  let full = withShown(order, shown);
  const moves = [];
  for (const parent of new Set(full.map(parentPath))) {
    const siblings = full.filter((p) => parentPath(p) === parent);
    const sorted = [...siblings].sort(compare);
    if (sorted.every((p, i) => p === siblings[i])) continue;
    full = placeSiblings(full, parent, sorted);
    moves.push({
      path: sorted[0]!,
      move: { direction: "up" as const, siblings: moveInto(sorted, sorted[0]!, "up"), order: full },
    });
  }
  return moves;
}

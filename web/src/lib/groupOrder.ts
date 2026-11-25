// The Custom order of Group headers is each profile's `groups.json` order, which the Daemon moves one sibling slot
// per `POST /api/reorder`.

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
      return l !== r ? l - r : compareNames(left[i]!, right[i]!);
    }
    return left.length - right.length;
  };
}

export interface GroupMove {
  direction: "up" | "down";
  /** Single-slot moves the Daemon must make. */
  steps: number;
  /** The profile's order once they land. */
  order: string[];
}

/**
 * The moves that put `active` next to its sibling `over`, after it when the drag went down. `shown` are the paths
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
  const listed = new Set(order);
  const full = [...order, ...[...new Set(shown)].filter((p) => !listed.has(p)).sort(compareNames)];
  const siblings = full.filter((p) => parentPath(p) === parent);
  if (!siblings.includes(active) || !siblings.includes(over)) return null;
  const moved = moveId(siblings, active, over, after);
  const from = siblings.indexOf(active);
  const to = moved.indexOf(active);
  if (from === to) return null;
  let next = 0;
  return {
    direction: to < from ? "up" : "down",
    steps: Math.abs(to - from),
    order: full.map((p) => (parentPath(p) === parent ? moved[next++]! : p)),
  };
}

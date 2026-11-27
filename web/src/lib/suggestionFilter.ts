import type { SuggestionItem } from "./types";

/** A selected chip: every key under an org, or one exact key. */
export type KeyChip = { org: string } | { key: string };

export interface KeyChipGroup {
  /** The part before the first `/`, or null for keys without one. */
  org: string | null;
  count: number;
  keys: { key: string; label: string; count: number }[];
}

/** Chips from the items' `mapping_key`s, grouped by the part before `/`, sorted by name. */
export function keyChipGroups(items: SuggestionItem[]): KeyChipGroup[] {
  // Keyed by `org/`, or by the whole key when it has no org, so the two never collide.
  const groups = new Map<string, KeyChipGroup>();
  for (const item of items) {
    const key = item.mapping_key;
    if (!key) continue;
    const slash = key.indexOf("/");
    const org = slash > 0 ? key.slice(0, slash) : null;
    const name = org === null ? key : `${org}/`;
    const group = groups.get(name) ?? { org, count: 0, keys: [] };
    group.count += 1;
    const entry = group.keys.find((k) => k.key === key);
    if (entry) entry.count += 1;
    else group.keys.push({ key, label: org === null ? key : key.slice(slash + 1), count: 1 });
    groups.set(name, group);
  }
  return [...groups.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([, g]) => ({ ...g, keys: g.keys.sort((a, b) => a.label.localeCompare(b.label)) }));
}

export const chipId = (chip: KeyChip) => ("org" in chip ? `org:${chip.org}` : `key:${chip.key}`);

function matchesChip(key: string, chip: KeyChip) {
  return "org" in chip ? key.startsWith(`${chip.org}/`) : key === chip.key;
}

function matchesQuery(item: SuggestionItem, query: string) {
  return [item.title, item.source_ref, item.reason, item.mapping_key ?? ""].some((field) =>
    field.toLowerCase().includes(query),
  );
}

/** Items matching the search (case-insensitive) and any selected chip; no chips selects all. */
export function filterSuggestions(items: SuggestionItem[], query: string, chips: KeyChip[]): SuggestionItem[] {
  const q = query.trim().toLowerCase();
  return items.filter(
    (item) =>
      (q === "" || matchesQuery(item, q)) &&
      (chips.length === 0 || (item.mapping_key != null && chips.some((c) => matchesChip(item.mapping_key!, c)))),
  );
}

import type { SuggestionItem, SuggestionMapping, SuggestionProject } from "./types";

export interface MappingProposal {
  provider_id: string;
  mapping_key: string;
  /** Current suggestions carrying this key. */
  count: number;
  /** The proposed Project path, or null when nothing matched. */
  project: string | null;
  /** `origin`: the Project's git origin is the key; `name`: its name or folder is the key's repo part. */
  match: "origin" | "name" | null;
}

const loose = (s: string) => s.toLowerCase().replace(/[-_]/g, "");
const folder = (path: string) => path.split(/[\\/]/).filter(Boolean).pop() ?? "";

function propose(key: string, projects: SuggestionProject[]): Pick<MappingProposal, "project" | "match"> {
  const wanted = key.toLowerCase();
  const byOrigin = projects.find((p) => p.origin?.toLowerCase() === wanted);
  if (byOrigin) return { project: byOrigin.path, match: "origin" };
  const repo = loose(key.slice(key.lastIndexOf("/") + 1));
  const byName = repo && projects.find((p) => loose(p.name) === repo || loose(folder(p.path)) === repo);
  if (byName) return { project: byName.path, match: "name" };
  return { project: null, match: null };
}

/** One proposal per `mapping_key` in `items` that has no stored mapping, sorted by key. */
export function proposeMappings(
  items: SuggestionItem[],
  mappings: SuggestionMapping[],
  projects: SuggestionProject[],
): MappingProposal[] {
  const mapped = new Set(mappings.map((m) => `${m.provider_id}\0${m.mapping_key}`));
  const counts = new Map<string, { provider_id: string; mapping_key: string; count: number }>();
  for (const item of items) {
    if (!item.mapping_key) continue;
    const id = `${item.provider_id}\0${item.mapping_key}`;
    if (mapped.has(id)) continue;
    const entry = counts.get(id) ?? { provider_id: item.provider_id, mapping_key: item.mapping_key, count: 0 };
    entry.count += 1;
    counts.set(id, entry);
  }
  return [...counts.values()]
    .sort((a, b) => a.mapping_key.localeCompare(b.mapping_key) || a.provider_id.localeCompare(b.provider_id))
    .map((entry) => ({ ...entry, ...propose(entry.mapping_key, projects) }));
}

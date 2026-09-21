// Skill provenance shared by the skills manager, slash-command picker, and skill tool card.

import type { SkillProvenance, SkillRoot, SkillsResponse } from "./api";

/** "multiple" when a name is backed by more than one distinct source. */
export type SkillSource = { kind: "single"; label: string; managed: boolean } | { kind: "multiple" };

export const AOE_MANAGED_LABEL = "AoE";

/** External skills use their root's label, falling back to the root id so a badge is never dropped. */
export function labelForProvenance(provenance: SkillProvenance, roots: SkillRoot[]): string {
  if (provenance.kind === "aoe-managed") return AOE_MANAGED_LABEL;
  return roots.find((root) => root.id === provenance.root)?.label ?? provenance.root;
}

export function badgeLabel(source: SkillSource): string {
  return source.kind === "single" ? source.label : "multiple sources";
}

/** Only an unambiguously AoE-managed skill is branded. */
export function badgeTone(source: SkillSource): "neutral" | "primary" {
  return source.kind === "single" && source.managed ? "primary" : "neutral";
}

export interface SkillIndex {
  /** Keyed by both directory and frontmatter name, which may diverge. */
  labelsByKey: Map<string, Set<string>>;
}

const EMPTY_INDEX: SkillIndex = { labelsByKey: new Map() };

/** A malformed response yields an empty index; the payload is unvalidated and badges are cosmetic. */
export function buildSkillIndex(res: SkillsResponse | null): SkillIndex {
  if (!res || !Array.isArray(res.skills)) return EMPTY_INDEX;
  const roots = Array.isArray(res.roots) ? res.roots : [];
  const labelsByKey = new Map<string, Set<string>>();
  for (const skill of res.skills) {
    // One malformed member must not cost every other skill its badge.
    if (!skill?.provenance) continue;
    const label = labelForProvenance(skill.provenance, roots);
    for (const key of [skill.directory, skill.name]) {
      if (!key) continue;
      const labels = labelsByKey.get(key) ?? new Set<string>();
      labels.add(label);
      labelsByKey.set(key, labels);
    }
  }
  return { labelsByKey };
}

/** Null when the name is not a known skill; one label reached via both keys counts once. */
export function resolveSkillSource(index: SkillIndex, commandName: string): SkillSource | null {
  const labels = index.labelsByKey.get(commandName);
  if (!labels || labels.size === 0) return null;
  if (labels.size > 1) return { kind: "multiple" };
  const [label] = labels;
  return { kind: "single", label: label!, managed: label === AOE_MANAGED_LABEL };
}

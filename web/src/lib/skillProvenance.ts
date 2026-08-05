// Skill provenance resolution shared by the skills manager, the composer's
// `/` slash-command picker, and the skill tool-call card (#3052). Pure, no
// React, so it can be unit-tested and reused by a plain hook.

import type { SkillProvenance, SkillRoot, SkillsResponse } from "./api";

/** What a resolved command/skill name should render as a badge: a single
 *  known source, or "multiple" when the same name is backed by more than one
 *  distinct source (the caller renders a generic "multiple sources" label). */
export type SkillSource = { kind: "single"; label: string; managed: boolean } | { kind: "multiple" };

/** The label AoE's own store renders as. Exported so a surface can ask whether
 *  a resolved source is the managed one without string-matching at the call
 *  site, and so renaming it is a one-line change. */
export const AOE_MANAGED_LABEL = "AoE";

/** The human-facing label for a skill's provenance. AoE-managed skills get a
 *  fixed short label; an external skill's label is its root's declared
 *  label, falling back to the raw root id when the root is unknown so a
 *  badge is never silently dropped. */
export function labelForProvenance(provenance: SkillProvenance, roots: SkillRoot[]): string {
  if (provenance.kind === "aoe-managed") return AOE_MANAGED_LABEL;
  return roots.find((root) => root.id === provenance.root)?.label ?? provenance.root;
}

/** The badge text for a resolved source. Lives here, not at each call site,
 *  because every surface showing this badge has to read identically; two
 *  inline ternaries would be two places for the ambiguous wording to drift. */
export function badgeLabel(source: SkillSource): string {
  return source.kind === "single" ? source.label : "multiple sources";
}

/** The badge tint for a resolved source. Only an unambiguously AoE-managed
 *  skill is branded: when a name resolves to several sources we cannot say the
 *  AoE one is what the agent will load, so claiming it in colour would be a
 *  guess dressed as a fact. */
export function badgeTone(source: SkillSource): "neutral" | "primary" {
  return source.kind === "single" && source.managed ? "primary" : "neutral";
}

export interface SkillIndex {
  /** Directory or frontmatter-name key -> distinct provenance labels backing
   *  it. AoE deliberately allows a skill's directory and frontmatter name to
   *  diverge, and agents advertise the frontmatter name in slash commands and
   *  tool calls, so every skill is indexed under both. */
  labelsByKey: Map<string, Set<string>>;
}

const EMPTY_INDEX: SkillIndex = { labelsByKey: new Map() };

/** Build a lookup from skill directory/name to the set of distinct
 *  provenance labels backing it. A null, absent, or malformed response yields
 *  an empty index, so every lookup resolves to null rather than throwing.
 *  The shape is checked rather than trusted because `fetchJson` casts an
 *  arbitrary 200 body to this type without validating it, and these badges are
 *  cosmetic: a surprising payload must not take out the surface rendering it. */
export function buildSkillIndex(res: SkillsResponse | null): SkillIndex {
  if (!res || !Array.isArray(res.skills)) return EMPTY_INDEX;
  const roots = Array.isArray(res.roots) ? res.roots : [];
  const labelsByKey = new Map<string, Set<string>>();
  for (const skill of res.skills) {
    // Skipped, not fatal: one malformed member must not cost every other skill
    // in the response its badge.
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

/** Resolve a slash-command or skill tool-call name to its provenance badge.
 *  Null means the name is not a known skill (every non-skill slash command,
 *  or an unrecognised skill name), so the caller renders no badge at all.
 *  Reaching the same label through both the directory key and the name key
 *  is one source, not two; only distinct labels count as ambiguous. */
export function resolveSkillSource(index: SkillIndex, commandName: string): SkillSource | null {
  const labels = index.labelsByKey.get(commandName);
  if (!labels || labels.size === 0) return null;
  if (labels.size > 1) return { kind: "multiple" };
  const [label] = labels;
  return { kind: "single", label: label!, managed: label === AOE_MANAGED_LABEL };
}

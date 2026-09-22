import type { SettingsFieldDescriptor } from "../../lib/types";
import type { TabId } from "../SettingsView";

// Section to settings tab; sections without a web tab are not searchable.
export const SECTION_TO_TAB: Record<string, TabId> = {
  session: "session",
  sandbox: "sandbox",
  worktree: "worktree",
  theme: "theme",
  sound: "sound",
  tmux: "tmux",
  updates: "updates",
  logging: "logging",
  web: "notifications",
  acp: "structured-view",
};

export interface SettingsSearchHit {
  section: string;
  field: string;
  tab: TabId;
  label: string;
  description: string;
  category: string;
  advanced: boolean;
  /** Text the fuzzy filter matches against: label, description, section, field. */
  searchText: string;
}

// Mirrors what SchemaSection renders: no `local_only` fields or tab-less sections.
export function buildSettingsSearchIndex(schema: SettingsFieldDescriptor[]): SettingsSearchHit[] {
  const hits: SettingsSearchHit[] = [];
  for (const d of schema) {
    if (d.web_write.policy === "local_only") continue;
    const tab = SECTION_TO_TAB[d.section];
    if (!tab) continue;
    hits.push({
      section: d.section,
      field: d.field,
      tab,
      label: d.label,
      description: d.description,
      category: d.category,
      advanced: d.advanced,
      searchText: `${d.label} ${d.description} ${d.section} ${d.field}`,
    });
  }
  return hits;
}

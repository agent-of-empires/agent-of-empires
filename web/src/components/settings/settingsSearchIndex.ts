import type { SettingsFieldDescriptor } from "../../lib/types";
import type { TabId } from "../SettingsView";

// Maps a schema section to the settings tab that renders it. Identity for most
// sections; `web` lives under the Notifications tab and `acp` under the
// Structured view tab (see renderTabContent in SettingsView). Sections absent
// here have no web tab, so their fields are excluded from search: a hit must be
// able to jump somewhere.
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
  /** Settings tab label shown as a badge next to the hit. */
  category: string;
  advanced: boolean;
  /** Text the fuzzy filter matches against: label, description, section, field. */
  searchText: string;
}

// Build the searchable settings index from the schema. Skips fields the
// dashboard cannot write (`local_only`, rejected by the server PATCH) and
// sections with no web tab, mirroring what SchemaSection actually renders.
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

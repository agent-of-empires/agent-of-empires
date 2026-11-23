import { PANEL_SETTING_FIELDS } from "../../lib/panelDefaults";
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
  systemd: "systemd",
  updates: "updates",
  serve: "updates",
  logging: "logging",
  web: "notifications",
  acp: "structured-view",
};

/** The tab that renders a field: its section's, unless the field is shown on another tab. */
export function tabOf(d: Pick<SettingsFieldDescriptor, "section" | "field">): TabId | undefined {
  if (d.section === "web" && (PANEL_SETTING_FIELDS as readonly string[]).includes(d.field)) return "panels";
  return SECTION_TO_TAB[d.section];
}

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
    const tab = tabOf(d);
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

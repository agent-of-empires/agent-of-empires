import type { SettingsFieldDescriptor } from "./types";

/** In display order, so every row reads the same. */
const TAG_RULES: [tag: string, applies: (d: SettingsFieldDescriptor) => boolean][] = [
  ["Needs restart", (d) => !!d.restart],
  ["Web dashboard only", (d) => !!d.web_only],
  ["Terminal app only", (d) => !!d.tui_only],
  // Every `acp` and `systemd` field qualifies; registry tests pin both sections' field lists.
  ["Structured view only", (d) => d.section === "acp"],
  ["Linux only", (d) => d.section === "systemd"],
];

export function settingTags(d: SettingsFieldDescriptor): string[] {
  return TAG_RULES.filter(([, applies]) => applies(d)).map(([tag]) => tag);
}

export const settingKey = (d: SettingsFieldDescriptor) => `${d.section}.${d.field}`;

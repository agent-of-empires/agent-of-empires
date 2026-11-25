import { createContext, useContext } from "react";

import type { SettingsResponse } from "./api";
import type { SettingsField } from "./generated/settingsKeys";

/** The Daemon's `web.auto_open_*` settings. */
export interface PanelDefaults {
  diff: boolean;
  terminal: boolean;
  plugins: boolean;
}

/** The `web` fields the Panels tab renders rather than the Notifications tab. */
export const PANEL_SETTING_FIELDS: readonly SettingsField<"web">[] = [
  "auto_open_diff_pane",
  "auto_open_terminal_pane",
  "auto_open_plugin_panes",
];

/** Matches the Daemon defaults, and stands in until the first settings read lands. */
const CLOSED: PanelDefaults = { diff: false, terminal: false, plugins: false };

export function parsePanelDefaults(settings: SettingsResponse | null | undefined): PanelDefaults {
  const web = settings?.web;
  return {
    diff: web?.auto_open_diff_pane === true,
    terminal: web?.auto_open_terminal_pane === true,
    plugins: web?.auto_open_plugin_panes === true,
  };
}

export const PanelDefaultsContext = createContext<PanelDefaults>(CLOSED);

export function usePanelDefaults(): PanelDefaults {
  return useContext(PanelDefaultsContext);
}

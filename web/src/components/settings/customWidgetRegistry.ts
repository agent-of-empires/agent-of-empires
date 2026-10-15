import { AcpDefaultsWidget } from "./AcpDefaultsWidget";
import type { CustomSettingsWidget } from "./customWidgets";
import {
  DefaultToolWidget,
  LoggingTargetsWidget,
  SmartRenameAgentWidget,
  SmartRenameModelWidget,
  SoundVolumeWidget,
  ThemeNameWidget,
} from "./customWidgets";

/** Custom settings controls by `widget.id`, mirroring src/tui/settings/fields.rs. */
export const CUSTOM_SETTINGS_WIDGETS: Record<string, CustomSettingsWidget> = {
  "theme-name": ThemeNameWidget,
  "default-tool": DefaultToolWidget,
  "smart-rename-agent": SmartRenameAgentWidget,
  "smart-rename-model": SmartRenameModelWidget,
  "sound-volume": SoundVolumeWidget,
  "logging-targets": LoggingTargetsWidget,
  "acp-defaults": AcpDefaultsWidget,
};

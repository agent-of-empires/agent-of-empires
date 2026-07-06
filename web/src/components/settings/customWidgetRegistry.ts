import { AcpDefaultsWidget } from "./AcpDefaultsWidget";
import type { CustomSettingsWidget } from "./customWidgets";
import {
  DefaultToolWidget,
  LoggingTargetsWidget,
  SmartRenameAgentWidget,
  SoundModeWidget,
  SoundVolumeWidget,
  ThemeNameWidget,
} from "./customWidgets";

/** Registry of bespoke settings controls keyed by `widget.id`, mirroring the
 *  TUI's custom-widget map (src/tui/settings/fields.rs). SchemaSection looks a
 *  field's `widget.id` up here when `widget.kind === "custom"`. Kept in a
 *  non-component module so the widget file stays Fast-Refresh clean. */
export const CUSTOM_SETTINGS_WIDGETS: Record<string, CustomSettingsWidget> = {
  "theme-name": ThemeNameWidget,
  "default-tool": DefaultToolWidget,
  "smart-rename-agent": SmartRenameAgentWidget,
  "sound-mode": SoundModeWidget,
  "sound-volume": SoundVolumeWidget,
  "logging-targets": LoggingTargetsWidget,
  "acp-defaults": AcpDefaultsWidget,
};

import { useWebSettings } from "../../hooks/useWebSettings";
import { FontSizeControl } from "./FontSizeControl";

/** Dashboard display preferences for the structured view. These are not agent
 *  config, so they live in `aoe-web-settings` rather than the ACP schema, and
 *  apply live to the conversation transcript only. Like the rest of that entry
 *  they are mirrored to the daemon's web-UI state by `webUiSync`, so they are
 *  not per-browser.
 *
 *  Values are read already clamped: `useWebSettings` normalizes both fields in
 *  `normalizeSnapshot`, so no re-clamping is needed here. */
export function StructuredViewDisplaySettings() {
  const { settings, update } = useWebSettings();

  return (
    <div className="space-y-4">
      <h3 className="font-mono text-sm uppercase tracking-widest text-text-muted">Conversation display</h3>

      <FontSizeControl
        label="Mobile font size"
        testIdPrefix="structured-mobile-font-size"
        value={settings.structuredMobileFontSize}
        onChange={(value) => update({ structuredMobileFontSize: value })}
        description="Font size for Structured View conversation content on mobile devices. Separate from the terminal font size, and shared with your other browsers like the rest of the dashboard preferences."
      />

      <FontSizeControl
        label="Desktop font size"
        testIdPrefix="structured-desktop-font-size"
        value={settings.structuredDesktopFontSize}
        onChange={(value) => update({ structuredDesktopFontSize: value })}
        description="Font size for Structured View conversation content on desktop devices. Separate from the terminal font size, and shared with your other browsers like the rest of the dashboard preferences."
      />
    </div>
  );
}

import { useWebSettings } from "../../hooks/useWebSettings";
import {
  MAX_PERSISTENT_STRUCTURED_VIEWS,
  MIN_PERSISTENT_STRUCTURED_VIEWS,
  normalizePersistentStructuredViewLimit,
} from "../../lib/persistentStructuredViews";
import { FontSizeControl } from "./FontSizeControl";

/** Structured view display preferences, stored in `aoe-web-settings` (already clamped). */
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

      <label className="flex items-center justify-between gap-3 cursor-pointer">
        <div>
          <div className="text-[13px] text-text-secondary">
            Keep conversations alive <span className="font-mono text-[11px] text-text-muted">(Beta)</span>
          </div>
          <p className="text-[11px] text-text-muted mt-1">
            Keep recently viewed structured views mounted so switching back is instant. Each one stays connected and
            uses more browser memory.
          </p>
        </div>
        <input
          type="checkbox"
          data-testid="persistent-structured-views-toggle"
          checked={settings.persistentStructuredViews}
          onChange={(e) => update({ persistentStructuredViews: e.target.checked })}
          className="accent-brand-600 w-4 h-4 shrink-0"
        />
      </label>

      {settings.persistentStructuredViews && (
        <div>
          <label htmlFor="persistent-structured-views-limit" className="block text-[13px] text-text-secondary mb-2">
            Conversation keep-alive limit
          </label>
          <input
            id="persistent-structured-views-limit"
            type="number"
            min={MIN_PERSISTENT_STRUCTURED_VIEWS}
            max={MAX_PERSISTENT_STRUCTURED_VIEWS}
            step={1}
            value={settings.maxPersistentStructuredViews}
            onChange={(e) =>
              update({ maxPersistentStructuredViews: normalizePersistentStructuredViewLimit(Number(e.target.value)) })
            }
            className="bg-surface-800 border border-surface-700 rounded-md px-2 py-1 text-sm text-text-primary font-mono w-16 text-center"
          />
        </div>
      )}
    </div>
  );
}

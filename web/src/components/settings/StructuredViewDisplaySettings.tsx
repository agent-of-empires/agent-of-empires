import { useWebSettings } from "../../hooks/useWebSettings";
import {
  MAX_PERSISTENT_STRUCTURED_VIEWS,
  MIN_PERSISTENT_STRUCTURED_VIEWS,
  normalizePersistentStructuredViewLimit,
} from "../../lib/persistentStructuredViews";
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
  const maxPersistentStructuredViews = normalizePersistentStructuredViewLimit(settings.maxPersistentStructuredViews);

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

      <div>
        <div className="space-y-3">
          <label className="flex items-center justify-between gap-3 cursor-pointer">
            <div>
              <div className="text-[13px] text-text-secondary">
                Keep recently viewed structured-view sessions loaded
              </div>
              <p className="text-[11px] text-text-muted mt-1">
                Keep recent structured-view sessions mounted for faster switching. Uses more browser memory and keeps
                extra WebSocket connections open.
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
              <label className="block text-[13px] text-text-secondary mb-2">Loaded session limit</label>
              <div className="flex items-center gap-3">
                <input
                  type="range"
                  min={MIN_PERSISTENT_STRUCTURED_VIEWS}
                  max={MAX_PERSISTENT_STRUCTURED_VIEWS}
                  step={1}
                  value={maxPersistentStructuredViews}
                  onChange={(e) =>
                    update({
                      maxPersistentStructuredViews: normalizePersistentStructuredViewLimit(Number(e.target.value)),
                    })
                  }
                  className="flex-1 accent-brand-600 h-1.5"
                />
                <input
                  type="number"
                  min={MIN_PERSISTENT_STRUCTURED_VIEWS}
                  max={MAX_PERSISTENT_STRUCTURED_VIEWS}
                  step={1}
                  data-testid="max-persistent-structured-views"
                  value={maxPersistentStructuredViews}
                  onChange={(e) =>
                    update({
                      maxPersistentStructuredViews: normalizePersistentStructuredViewLimit(Number(e.target.value)),
                    })
                  }
                  className="bg-surface-800 border border-surface-700 rounded-md px-2 py-1 text-sm text-text-primary font-mono w-16 text-center"
                />
              </div>
              <p className="text-[11px] text-text-muted mt-1">
                Higher limits improve switching across large workspaces but keep more assistant-ui runtimes and
                WebSockets alive.
              </p>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

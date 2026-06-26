import { useCallback, useEffect, useState } from "react";

import { fetchPlugins, setPluginEnabled, type PluginListResponse, type PluginView } from "../../lib/api";
import { reportInfo } from "../../lib/toastBus";

/// Plugin management: list every known plugin (name, version, description,
/// validation provenance, capabilities, and enabled / approval state) and
/// toggle it on or off. Installing and capability approval are CLI-driven (`aoe plugin
/// install`); this panel shows the resulting state. The toggle POSTs to
/// `/api/plugins/{id}/enabled`; it is a host mutation, so it needs read-write
/// mode and (when login is enabled) an elevated session. A `403
/// elevation_required` response pops the global passphrase prompt via the
/// fetch interceptor, the same as any other elevated setting; other failures
/// surface their message inline. `load_errors` are shown as a warning line.
export function PluginsSettings() {
  const [data, setData] = useState<PluginListResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(async () => {
    const next = await fetchPlugins();
    if (next) {
      setData(next);
      setError(null);
    } else {
      setError("Failed to load plugins.");
    }
  }, []);

  useEffect(() => {
    // Deferred a tick: the lint forbids synchronous setState chains inside
    // an effect body (same pattern as SettingsView's schema load).
    const timer = setTimeout(() => {
      void reload();
    }, 0);
    return () => clearTimeout(timer);
  }, [reload]);

  const onToggle = async (plugin: PluginView, enabled: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const result = await setPluginEnabled(plugin.id, enabled);
      if (result.kind === "ok") {
        // The server returns the refreshed list, so adopt it directly.
        setData(result.data);
        // The serve gate is startup-only: disabling aoe.web rewrites config
        // but the running daemon keeps serving until it restarts. Say so,
        // otherwise the toggle looks like a no-op (#2311 testing feedback).
        if (plugin.id === "aoe.web" && !enabled) {
          reportInfo("Web dashboard stays up until aoe serve is restarted.");
        }
      } else {
        // The toggle did not take effect; the checkbox is controlled by the
        // unchanged `plugin.enabled`, so the existing `data` already reflects
        // the server. Just surface the message.
        setError(result.message);
      }
    } finally {
      setBusy(false);
    }
  };

  if (!data && !error) {
    return <p className="text-sm text-text-dim">Loading plugins…</p>;
  }

  return (
    <div className="space-y-4">
      {error && <p className="text-sm text-status-error">{error}</p>}

      {data && data.load_errors.length > 0 && (
        <div className="rounded border border-status-warning bg-status-warning/10 p-3 text-xs text-status-warning">
          <p className="mb-1 font-semibold">Plugin load problems</p>
          {data.load_errors.map((e) => (
            <p key={e}>{e}</p>
          ))}
        </div>
      )}

      <div className="space-y-3">
        {data && data.plugins.length === 0 && (
          <p className="text-xs text-text-dim" data-testid="plugins-empty">
            No plugins detected.
          </p>
        )}
        {data?.plugins.map((plugin) => (
          <div
            key={plugin.id}
            className="rounded border border-surface-700 bg-surface-850 p-3"
            data-testid={`plugin-${plugin.id}`}
          >
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium">{plugin.name}</span>
                  <span className="text-xs text-text-dim">v{plugin.version}</span>
                  <span
                    className="rounded bg-accent-500/20 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-accent-500"
                    data-testid={`plugin-validation-${plugin.id}`}
                  >
                    {plugin.validation}
                  </span>
                  {plugin.needs_reapproval && (
                    <span
                      className="rounded bg-status-warning/20 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-status-warning"
                      data-testid={`plugin-needs-approval-${plugin.id}`}
                    >
                      needs approval
                    </span>
                  )}
                </div>
                <p className="mt-1 text-xs text-text-dim">{plugin.description}</p>
                {plugin.capabilities.length > 0 && (
                  <p className="mt-1 text-[11px] text-text-dim">
                    Capabilities: {plugin.capabilities.join(", ")}
                    {plugin.granted ? "" : " (not granted)"}
                  </p>
                )}
                {(plugin.ui_contributions ?? []).length > 0 && (
                  <p className="mt-1 text-[11px] text-text-dim">
                    UI: {[...new Set((plugin.ui_contributions ?? []).map((u) => u.slot))].join(", ")}
                  </p>
                )}
                {plugin.needs_reapproval && (
                  <p className="mt-1 text-[11px] text-status-warning">
                    Installed but inactive. Re-approve with <code>aoe plugin update {plugin.id}</code>.
                  </p>
                )}
              </div>
              <label className="flex shrink-0 items-center gap-1 text-xs">
                <input
                  type="checkbox"
                  role="switch"
                  aria-label={`Enable ${plugin.name}`}
                  checked={plugin.enabled}
                  disabled={busy}
                  onChange={(e) => void onToggle(plugin, e.target.checked)}
                />
                Enabled
              </label>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

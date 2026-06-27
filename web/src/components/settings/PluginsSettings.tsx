import { useCallback, useEffect, useState } from "react";

import {
  discoverPlugins,
  fetchPluginUpdates,
  fetchPlugins,
  setPluginEnabled,
  type PluginDiscoveryResult,
  type PluginListResponse,
  type PluginUpdateStatus,
  type PluginView,
} from "../../lib/api";
import { reportInfo } from "../../lib/toastBus";
import { PluginDetailModal } from "./PluginDetailModal";

interface DetailTarget {
  source: string;
  title: string;
  fallback?: {
    version?: string;
    description?: string;
    capabilities?: string[];
    ui_contributions?: { slot: string; id: string }[];
  };
  installCommand?: string;
}

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

  // Update checks (on-demand, never auto). Keyed by plugin id.
  const [updates, setUpdates] = useState<Record<string, PluginUpdateStatus>>({});
  const [checkingUpdates, setCheckingUpdates] = useState(false);

  // GitHub discovery (browse-only; install is CLI).
  const [discoverQuery, setDiscoverQuery] = useState("");
  const [discoverResults, setDiscoverResults] = useState<PluginDiscoveryResult[] | null>(null);
  const [discoverError, setDiscoverError] = useState<string | null>(null);
  const [discovering, setDiscovering] = useState(false);

  // The plugin whose detail modal is open (null = closed).
  const [detail, setDetail] = useState<DetailTarget | null>(null);

  // Two tabs, JetBrains-style: manage installed plugins vs browse the
  // marketplace (GitHub discovery).
  const [tab, setTab] = useState<"installed" | "marketplace">("installed");

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

  const onCheckUpdates = async () => {
    setCheckingUpdates(true);
    setError(null);
    try {
      const res = await fetchPluginUpdates();
      if (res.kind === "ok") {
        const next: Record<string, PluginUpdateStatus> = {};
        for (const s of res.updates) next[s.id] = s;
        setUpdates(next);
      } else {
        // Clear stale badges and surface the failure, so the button is not a
        // silent no-op.
        setUpdates({});
        setError(res.message);
      }
    } finally {
      setCheckingUpdates(false);
    }
  };

  const onDiscover = async () => {
    setDiscovering(true);
    setDiscoverError(null);
    const res = await discoverPlugins(discoverQuery);
    if (res.kind === "ok") {
      setDiscoverResults(res.results);
    } else {
      setDiscoverResults(null);
      setDiscoverError(res.message);
    }
    setDiscovering(false);
  };

  if (!data && !error) {
    return <p className="text-sm text-text-dim">Loading plugins…</p>;
  }

  return (
    <div className="space-y-4">
      <div role="tablist" className="flex gap-1 border-b border-surface-700">
        {(["installed", "marketplace"] as const).map((t) => (
          <button
            key={t}
            type="button"
            role="tab"
            aria-selected={tab === t}
            onClick={() => setTab(t)}
            data-testid={`plugins-tab-${t}`}
            className={`px-3 py-1.5 text-xs capitalize ${
              tab === t ? "border-b-2 border-accent-500 font-medium text-accent-500" : "text-text-dim"
            }`}
          >
            {t}
          </button>
        ))}
      </div>

      {tab === "marketplace" && (
        <div className="space-y-2">
          <div className="flex flex-wrap items-center gap-2">
            <input
              type="search"
              value={discoverQuery}
              onChange={(e) => setDiscoverQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void onDiscover();
              }}
              placeholder="Search GitHub (aoe-plugin topic)…"
              className="min-w-0 flex-1 rounded border border-surface-700 bg-surface-850 px-2 py-1 text-xs"
              data-testid="plugins-discover-query"
            />
            <button
              type="button"
              className="rounded border border-surface-700 px-2 py-1 text-xs hover:bg-surface-800 disabled:opacity-50"
              disabled={discovering}
              onClick={() => void onDiscover()}
              data-testid="plugins-discover"
            >
              {discovering ? "Searching…" : "Search GitHub"}
            </button>
          </div>

          {discoverError && (
            <p className="text-xs text-status-error" data-testid="plugins-discover-error">
              {discoverError}
            </p>
          )}

          {discoverResults && (
            <div className="space-y-2" data-testid="plugins-discover-results">
              {discoverResults.length === 0 ? (
                <p className="text-xs text-text-dim">No plugins found on the aoe-plugin topic.</p>
              ) : (
                discoverResults.map((r) => (
                  <div
                    key={r.slug}
                    className="rounded border border-surface-700 bg-surface-850 p-2 text-xs"
                    data-testid={`plugins-discover-result-${r.slug}`}
                  >
                    <div className="flex flex-wrap items-center gap-2">
                      <button
                        type="button"
                        className="font-medium text-accent-500 hover:underline"
                        onClick={() => setDetail({ source: r.slug, title: r.slug, installCommand: r.install_command })}
                        data-testid={`plugins-discover-open-${r.slug}`}
                      >
                        {r.slug}
                      </button>
                      <span className="rounded bg-accent-500/20 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-accent-500">
                        {r.badge}
                      </span>
                      <span className="text-text-dim">★ {r.stars}</span>
                      <a href={r.html_url} target="_blank" rel="noreferrer" className="text-text-dim hover:underline">
                        GitHub ↗
                      </a>
                    </div>
                    {r.description && <p className="mt-1 text-text-dim">{r.description}</p>}
                    <p className="mt-1 text-text-dim">
                      Install in a terminal: <code>{r.install_command}</code>
                    </p>
                  </div>
                ))
              )}
            </div>
          )}
        </div>
      )}

      {tab === "installed" && (
        <div className="space-y-3">
          {error && <p className="text-sm text-status-error">{error}</p>}

          {data && data.load_errors.length > 0 && (
            <div className="rounded border border-status-warning bg-status-warning/10 p-3 text-xs text-status-warning">
              <p className="mb-1 font-semibold">Plugin load problems</p>
              {data.load_errors.map((e) => (
                <p key={e}>{e}</p>
              ))}
            </div>
          )}

          <button
            type="button"
            className="rounded border border-surface-700 px-2 py-1 text-xs hover:bg-surface-800 disabled:opacity-50"
            disabled={checkingUpdates}
            onClick={() => void onCheckUpdates()}
            data-testid="plugins-check-updates"
          >
            {checkingUpdates ? "Checking…" : "Check for updates"}
          </button>

          {data && data.plugins.length === 0 && (
            <p className="text-xs text-text-dim" data-testid="plugins-empty">
              No plugins detected.
            </p>
          )}
          {data?.plugins.map((plugin) => {
            const update = updates[plugin.id];
            return (
              <div
                key={plugin.id}
                className="rounded border border-surface-700 bg-surface-850 p-3"
                data-testid={`plugin-${plugin.id}`}
              >
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <button
                        type="button"
                        className="font-medium hover:underline"
                        onClick={() =>
                          setDetail({
                            source: plugin.source ?? "",
                            title: plugin.name,
                            fallback: {
                              version: plugin.version,
                              description: plugin.description,
                              capabilities: plugin.capabilities,
                              ui_contributions: plugin.ui_contributions,
                            },
                          })
                        }
                        data-testid={`plugin-open-${plugin.id}`}
                      >
                        {plugin.name}
                      </button>
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
                      {update?.needs_update && (
                        <span
                          className="rounded bg-accent-500/20 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-accent-500"
                          data-testid={`plugin-update-available-${plugin.id}`}
                        >
                          update available
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
                    {update?.needs_update && (
                      <p className="mt-1 text-[11px] text-text-dim">
                        Update available ({update.current} → {update.available ?? "modified"}). Update with{" "}
                        <code>aoe plugin update {plugin.id}</code>.
                      </p>
                    )}
                    {update?.error && (
                      <p className="mt-1 text-[11px] text-status-error">Update check failed: {update.error}</p>
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
            );
          })}
        </div>
      )}

      {detail && (
        <PluginDetailModal
          key={detail.source}
          source={detail.source}
          title={detail.title}
          fallback={detail.fallback}
          installCommand={detail.installCommand}
          onClose={() => setDetail(null)}
        />
      )}
    </div>
  );
}

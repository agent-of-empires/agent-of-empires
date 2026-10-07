import { useCallback, useEffect, useState } from "react";

import {
  fetchPlugins,
  setPluginEnabled,
  startPluginUninstall,
  type PluginListResponse,
  type PluginView,
} from "../../lib/api";
import { reportInfo } from "../../lib/toastBus";
import { PluginCard } from "./PluginCard";
import { PluginDetailModal } from "./PluginDetailModal";
import { PluginInstallConsentModal } from "./PluginInstallConsentModal";
import { PluginJobProgressModal } from "./PluginJobProgressModal";
import { PluginMarketplace } from "./PluginMarketplace";
import { PluginUpdateConsentModal } from "./PluginUpdateConsentModal";
import { usePluginMarketplace, usePluginUpdates, type PluginJobRef } from "./pluginFlows";

type DetailTarget = Pick<Parameters<typeof PluginDetailModal>[0], "source" | "title" | "fallback" | "installCommand">;

/** Plugin management: installed list with toggles and lifecycle jobs, plus the
 *  marketplace. Mutations need an elevated session; the fetch interceptor
 *  handles `403 elevation_required`. */
export function PluginsSettings({ readOnly = false }: { readOnly?: boolean } = {}) {
  const [data, setData] = useState<PluginListResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [tab, setTab] = useState<"installed" | "marketplace">("installed");
  const [detail, setDetail] = useState<DetailTarget | null>(null);
  const [confirmUninstall, setConfirmUninstall] = useState<PluginView | null>(null);
  const [job, setJob] = useState<PluginJobRef | null>(null);
  const updates = usePluginUpdates(setError, setJob);
  const market = usePluginMarketplace(setJob);

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
    // Deferred so the effect body does not set state synchronously.
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
        setData(result.data);
        // The serve gate is read at startup, so the dashboard keeps running.
        if (plugin.id === "aoe.web" && !enabled) {
          reportInfo("Web dashboard stays up until aoe serve is restarted.");
        }
      } else {
        setError(result.message);
      }
    } finally {
      setBusy(false);
    }
  };

  const onConfirmUninstall = async () => {
    if (!confirmUninstall) return;
    const plugin = confirmUninstall;
    setConfirmUninstall(null);
    const res = await startPluginUninstall(plugin.id);
    if (res.kind === "ok") setJob({ id: res.jobId, title: `Uninstalling ${plugin.name}` });
    else setError(res.message);
  };

  if (!data && !error) {
    return <p className="text-sm text-text-dim">Loading plugins…</p>;
  }

  const review = updates.review;
  return (
    <div className="space-y-4">
      {/* Read-only (CityHall) hides the marketplace and every lifecycle control. */}
      {!readOnly && (
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
      )}

      {tab === "marketplace" && (
        <PluginMarketplace
          market={market}
          installedSources={new Set(data?.plugins.map((p) => p.source))}
          onOpenDetail={setDetail}
        />
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

          {!readOnly && (
            <button
              type="button"
              className="rounded border border-surface-700 px-2 py-1 text-xs hover:bg-surface-800 disabled:opacity-50"
              disabled={updates.checking}
              onClick={() => void updates.check()}
              data-testid="plugins-check-updates"
            >
              {updates.checking ? "Checking…" : "Check for updates"}
            </button>
          )}

          {data && data.plugins.length === 0 && (
            <p className="text-xs text-text-dim" data-testid="plugins-empty">
              No plugins detected.
            </p>
          )}
          {data?.plugins.map((plugin) => (
            <PluginCard
              key={plugin.id}
              plugin={plugin}
              update={updates.updates[plugin.id]}
              readOnly={readOnly}
              toggleBusy={busy}
              updating={updates.updatingId === plugin.id}
              onOpen={() =>
                setDetail({
                  source: plugin.source ?? "",
                  title: plugin.name,
                  fallback: {
                    version: plugin.version,
                    description: plugin.description,
                    capabilities: plugin.capabilities,
                    ui_contributions: plugin.ui_contributions,
                    icon: plugin.icon,
                    icon_asset_url: plugin.icon_asset_url,
                  },
                })
              }
              onToggle={(enabled) => void onToggle(plugin, enabled)}
              onUpdate={() => void updates.preview(plugin)}
              onUninstall={() => setConfirmUninstall(plugin)}
            />
          ))}
        </div>
      )}

      {detail && <PluginDetailModal key={detail.source} {...detail} onClose={() => setDetail(null)} />}

      {review && (
        <PluginUpdateConsentModal
          key={review.plugin.id}
          consent={review.consent}
          name={review.plugin.name}
          fromVersion={review.fromVersion}
          toVersion={review.toVersion}
          changelog={review.changelog}
          busy={updates.busy}
          error={updates.reviewError}
          onApprove={() => void updates.approve()}
          onDecline={review.consent ? () => void updates.decline() : undefined}
          onClose={updates.closeReview}
        />
      )}

      {market.consent && (
        <PluginInstallConsentModal
          key={market.consent.fingerprint}
          consent={market.consent}
          busy={market.installBusy}
          error={market.installError}
          onApprove={() => void market.approveInstall()}
          onClose={market.closeConsent}
        />
      )}

      {confirmUninstall && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
          role="dialog"
          aria-modal="true"
          aria-label={`Uninstall ${confirmUninstall.name}`}
          onClick={() => setConfirmUninstall(null)}
          data-testid="plugin-uninstall-confirm"
        >
          <div
            className="w-full max-w-sm rounded border border-surface-700 bg-surface-900 p-4 text-sm"
            onClick={(e) => e.stopPropagation()}
          >
            <h2 className="mb-2 font-semibold">Uninstall {confirmUninstall.name}?</h2>
            <p className="mb-4 text-xs text-text-dim">
              This removes the plugin's files, config entry, and lockfile entry from the host. You can reinstall it
              later from the marketplace.
            </p>
            <div className="flex justify-end gap-2">
              <button
                type="button"
                className="rounded border border-surface-700 px-3 py-1 text-xs hover:bg-surface-800"
                onClick={() => setConfirmUninstall(null)}
                data-testid="plugin-uninstall-cancel"
              >
                Cancel
              </button>
              <button
                type="button"
                className="rounded bg-status-error px-3 py-1 text-xs font-medium text-white hover:opacity-90"
                onClick={() => void onConfirmUninstall()}
                data-testid="plugin-uninstall-confirm-button"
              >
                Uninstall
              </button>
            </div>
          </div>
        </div>
      )}

      {job && (
        <PluginJobProgressModal
          jobId={job.id}
          title={job.title}
          onClose={() => {
            setJob(null);
            void reload();
          }}
        />
      )}
    </div>
  );
}

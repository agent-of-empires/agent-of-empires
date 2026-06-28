import { useCallback, useEffect, useState } from "react";

import {
  applyPluginUpdate,
  dismissPluginUpdate,
  discoverPlugins,
  fetchPluginUpdates,
  fetchPlugins,
  previewPluginInstall,
  previewPluginUpdate,
  setPluginEnabled,
  startPluginInstall,
  startPluginUninstall,
  type PluginDiscoveryResult,
  type PluginInstallConsent,
  type PluginListResponse,
  type PluginUpdateConsent,
  type PluginUpdateStatus,
  type PluginView,
} from "../../lib/api";
import { reportInfo } from "../../lib/toastBus";
import { PluginDetailModal } from "./PluginDetailModal";
import { PluginInstallConsentModal } from "./PluginInstallConsentModal";
import { PluginJobProgressModal } from "./PluginJobProgressModal";
import { PluginUpdateConsentModal } from "./PluginUpdateConsentModal";

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
/// validation provenance, capabilities, and enabled / approval state), toggle
/// it on or off, and run the lifecycle actions (install from the marketplace,
/// update, uninstall) as host-side jobs with a live log tail. Install and
/// uninstall still show the same capability disclosure the CLI prompts for. The
/// mutations are host operations, so they need read-write mode and (when login
/// is enabled) an elevated session. A `403 elevation_required` response pops the
/// global passphrase prompt via the fetch interceptor, the same as any other
/// elevated setting; other failures surface their message inline. `load_errors`
/// are shown as a warning line.
export function PluginsSettings() {
  const [data, setData] = useState<PluginListResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Update checks (on-demand, never auto). Keyed by plugin id.
  const [updates, setUpdates] = useState<Record<string, PluginUpdateStatus>>({});
  const [checkingUpdates, setCheckingUpdates] = useState(false);

  // GitHub discovery (the marketplace tab; each result can install in-app).
  const [discoverQuery, setDiscoverQuery] = useState("");
  const [discoverResults, setDiscoverResults] = useState<PluginDiscoveryResult[] | null>(null);
  const [discoverError, setDiscoverError] = useState<string | null>(null);
  const [discovering, setDiscovering] = useState(false);

  // The plugin whose detail modal is open (null = closed).
  const [detail, setDetail] = useState<DetailTarget | null>(null);

  // The in-app update flow: which plugin's Update button is previewing, and the
  // consent modal when a fetched update needs explicit approval.
  const [updatingId, setUpdatingId] = useState<string | null>(null);
  const [consentModal, setConsentModal] = useState<{ plugin: PluginView; consent: PluginUpdateConsent } | null>(null);
  const [consentBusy, setConsentBusy] = useState(false);
  const [consentError, setConsentError] = useState<string | null>(null);

  // The in-app install flow: which marketplace source is previewing, the
  // consent modal once its disclosure is fetched, and busy/error while the
  // install job is started.
  const [previewingSource, setPreviewingSource] = useState<string | null>(null);
  const [installConsent, setInstallConsent] = useState<PluginInstallConsent | null>(null);
  const [installBusy, setInstallBusy] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);

  // An installed external plugin awaiting uninstall confirmation.
  const [confirmUninstall, setConfirmUninstall] = useState<PluginView | null>(null);

  // The active lifecycle job (install / update / uninstall) the progress modal
  // is following; null when none is running.
  const [job, setJob] = useState<{ id: string; title: string } | null>(null);

  const clearUpdateBadge = (id: string) =>
    setUpdates((u) => {
      const next = { ...u };
      delete next[id];
      return next;
    });

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

  // Drive an in-app update: preview first, then apply a safe update directly or
  // open the consent modal when the fetched version expands access.
  const onUpdate = async (plugin: PluginView) => {
    setUpdatingId(plugin.id);
    setError(null);
    try {
      const res = await previewPluginUpdate(plugin.id);
      if (res.kind !== "ok") {
        setError(res.message);
        return;
      }
      const preview = res.preview;
      if (preview.kind === "no_update") {
        reportInfo(`${plugin.name} is already up to date.`);
        clearUpdateBadge(plugin.id);
      } else if (preview.kind === "safe_update") {
        const started = await applyPluginUpdate(plugin.id, preview.fingerprint);
        if (started.kind === "ok") {
          clearUpdateBadge(plugin.id);
          setJob({ id: started.jobId, title: `Updating ${plugin.name}` });
        } else {
          setError(started.message);
        }
      } else {
        setConsentError(null);
        setConsentModal({ plugin, consent: preview.consent });
      }
    } finally {
      setUpdatingId(null);
    }
  };

  const onApproveUpdate = async () => {
    if (!consentModal) return;
    setConsentBusy(true);
    setConsentError(null);
    try {
      const res = await applyPluginUpdate(consentModal.plugin.id, consentModal.consent.fingerprint);
      if (res.kind === "ok") {
        clearUpdateBadge(consentModal.plugin.id);
        const name = consentModal.plugin.name;
        setConsentModal(null);
        setJob({ id: res.jobId, title: `Updating ${name}` });
      } else {
        // Any failure keeps the modal open with the message; the user can close
        // and re-Update to re-preview.
        setConsentError(res.message);
      }
    } finally {
      setConsentBusy(false);
    }
  };

  const onDeclineUpdate = async () => {
    if (!consentModal) return;
    setConsentBusy(true);
    setConsentError(null);
    try {
      // The current version stays active either way, but only clear local state
      // once the backend actually recorded the decline; otherwise a failed
      // dismiss would look persisted and the prompt would return on reload.
      const res = await dismissPluginUpdate(consentModal.plugin.id, consentModal.consent.fingerprint);
      if (res.kind === "ok") {
        clearUpdateBadge(consentModal.plugin.id);
        setConsentModal(null);
      } else {
        setConsentError(res.message);
      }
    } finally {
      setConsentBusy(false);
    }
  };

  // The `gh:owner/repo` source to install, taken from the discovery row's copy
  // command (`aoe plugin install gh:owner/repo`) so it always carries the `gh:`
  // prefix the web install path requires.
  const sourceFromCommand = (command: string) => command.replace(/^.*\binstall\s+/, "").trim();

  // Marketplace install: preview the disclosure first, then open the consent
  // modal. Nothing is installed until the user approves.
  const onInstall = async (source: string) => {
    setPreviewingSource(source);
    setDiscoverError(null);
    setInstallError(null);
    try {
      const res = await previewPluginInstall(source);
      if (res.kind === "ok") {
        setInstallConsent(res.consent);
      } else {
        setDiscoverError(res.message);
      }
    } finally {
      setPreviewingSource(null);
    }
  };

  const onApproveInstall = async () => {
    if (!installConsent) return;
    setInstallBusy(true);
    setInstallError(null);
    try {
      const res = await startPluginInstall(installConsent.source, installConsent.fingerprint);
      if (res.kind === "ok") {
        const title = `Installing ${installConsent.id}`;
        setInstallConsent(null);
        setJob({ id: res.jobId, title });
      } else {
        setInstallError(res.message);
      }
    } finally {
      setInstallBusy(false);
    }
  };

  const onConfirmUninstall = async () => {
    if (!confirmUninstall) return;
    const plugin = confirmUninstall;
    setConfirmUninstall(null);
    const res = await startPluginUninstall(plugin.id);
    if (res.kind === "ok") {
      setJob({ id: res.jobId, title: `Uninstalling ${plugin.name}` });
    } else {
      setError(res.message);
    }
  };

  // When a job modal closes (terminal state), refresh the list so the installed
  // set, versions, and approval state reflect what the job did.
  const onJobClose = async () => {
    setJob(null);
    await reload();
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
                    <div className="mt-2 flex flex-wrap items-center gap-2">
                      {r.badge === "installed" ||
                      data?.plugins.some((p) => p.source === sourceFromCommand(r.install_command)) ? (
                        <span className="text-text-dim">Installed.</span>
                      ) : (
                        <button
                          type="button"
                          className="rounded bg-brand-600 px-2 py-0.5 text-[11px] font-medium text-white hover:bg-brand-500 disabled:opacity-50"
                          disabled={previewingSource !== null}
                          onClick={() => void onInstall(sourceFromCommand(r.install_command))}
                          data-testid={`plugins-install-${r.slug}`}
                        >
                          {previewingSource === sourceFromCommand(r.install_command) ? "Checking…" : "Install"}
                        </button>
                      )}
                      <span className="text-text-dim">
                        or in a terminal: <code>{r.install_command}</code>
                      </span>
                    </div>
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
                      <div className="mt-1 flex flex-wrap items-center gap-2">
                        <span className="text-[11px] text-text-dim">
                          Update available ({update.current} → {update.available ?? "modified"}).
                        </span>
                        <button
                          type="button"
                          className="rounded border border-surface-700 px-2 py-0.5 text-[11px] hover:bg-surface-800 disabled:opacity-50"
                          disabled={updatingId === plugin.id}
                          onClick={() => void onUpdate(plugin)}
                          data-testid={`plugin-update-${plugin.id}`}
                        >
                          {updatingId === plugin.id ? "Checking…" : "Update"}
                        </button>
                      </div>
                    )}
                    {update?.error && (
                      <p className="mt-1 text-[11px] text-status-error">Update check failed: {update.error}</p>
                    )}
                  </div>
                  <div className="flex shrink-0 flex-col items-end gap-2">
                    <label className="flex items-center gap-1 text-xs">
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
                    {!plugin.builtin && plugin.source && (
                      <button
                        type="button"
                        className="rounded border border-status-error/50 px-2 py-0.5 text-[11px] text-status-error hover:bg-status-error/10 disabled:opacity-50"
                        onClick={() => setConfirmUninstall(plugin)}
                        data-testid={`plugin-uninstall-${plugin.id}`}
                      >
                        Uninstall
                      </button>
                    )}
                  </div>
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

      {consentModal && (
        <PluginUpdateConsentModal
          key={consentModal.plugin.id}
          consent={consentModal.consent}
          name={consentModal.plugin.name}
          busy={consentBusy}
          error={consentError}
          onApprove={() => void onApproveUpdate()}
          onDecline={() => void onDeclineUpdate()}
          onClose={() => setConsentModal(null)}
        />
      )}

      {installConsent && (
        <PluginInstallConsentModal
          key={installConsent.fingerprint}
          consent={installConsent}
          busy={installBusy}
          error={installError}
          onApprove={() => void onApproveInstall()}
          onClose={() => setInstallConsent(null)}
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

      {job && <PluginJobProgressModal jobId={job.id} title={job.title} onClose={() => void onJobClose()} />}
    </div>
  );
}

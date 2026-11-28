import { useCallback, useEffect, useRef, useState } from "react";

import {
  fetchPlugins,
  setPluginEnabled,
  startPluginUninstall,
  type PluginListResponse,
  type PluginView,
} from "../../lib/api";
import { BACKDROP, BUTTON_DANGER, BUTTON_GHOST, BUTTON_SECONDARY, DIALOG } from "../../lib/surfaces";
import { CancelButton, ConfirmButton, Dialog } from "../Dialog";
import { Notice, PanelMessage } from "../panelParts";
import { useConfirmKeys, useDialogFocus } from "../dialogHooks";
import { PluginCard } from "./PluginCard";
import { PluginDetailModal } from "./PluginDetailModal";
import { PluginInstallConsentModal } from "./PluginInstallConsentModal";
import { PluginJobProgressModal } from "./PluginJobProgressModal";
import { PluginMarketplace } from "./PluginMarketplace";
import { PluginUpdateConsentModal } from "./PluginUpdateConsentModal";
import { usePluginMarketplace, usePluginUpdates, type PluginJobRef } from "./pluginFlows";

const WEB_PLUGIN_ID = "aoe.web";

type DetailTarget = Pick<Parameters<typeof PluginDetailModal>[0], "source" | "title" | "fallback" | "installCommand">;

/** Plugin management: installed list with toggles and lifecycle jobs, plus the
 *  marketplace. Mutations need an elevated session; the fetch interceptor
 *  handles `403 elevation_required`. */
export function PluginsSettings({
  readOnly = false,
  initialMarketplaceFilter,
}: { readOnly?: boolean; initialMarketplaceFilter?: "suggestions" } = {}) {
  const [data, setData] = useState<PluginListResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [tab, setTab] = useState<"installed" | "marketplace">(
    initialMarketplaceFilter && !readOnly ? "marketplace" : "installed",
  );
  const [detail, setDetail] = useState<DetailTarget | null>(null);
  const [confirmUninstall, setConfirmUninstall] = useState<PluginView | null>(null);
  const uninstallDialogRef = useRef<HTMLDivElement | null>(null);
  const [confirmWebOff, setConfirmWebOff] = useState<PluginView | null>(null);
  const [job, setJob] = useState<PluginJobRef | null>(null);
  const updates = usePluginUpdates(setError, setJob);
  const market = usePluginMarketplace(setJob, initialMarketplaceFilter === "suggestions");
  const { discover } = market;

  useEffect(() => {
    if (!initialMarketplaceFilter || readOnly) return;
    const timer = setTimeout(() => {
      void discover();
    }, 0);
    return () => clearTimeout(timer);
    // Once on mount; the cleared timer makes a StrictMode remount fire it once.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

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

  useEffect(() => {
    if (!confirmUninstall) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const dialog = e
        .composedPath()
        .find((node) => node instanceof HTMLElement && node.getAttribute("aria-modal") === "true");
      if (dialog && dialog !== uninstallDialogRef.current) return;
      setConfirmUninstall(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [confirmUninstall]);

  const onToggle = async (plugin: PluginView, enabled: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const result = await setPluginEnabled(plugin.id, enabled);
      if (result.kind === "ok") {
        setData(result.data);
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
    return <PanelMessage className="px-1">Loading plugins…</PanelMessage>;
  }

  const review = updates.review;
  return (
    <div className="space-y-4">
      {/* Read-only (CityHall) hides the marketplace and every lifecycle control. */}
      {!readOnly && (
        <div role="tablist" className="flex gap-4 border-b border-surface-700/50 px-1">
          {(["installed", "marketplace"] as const).map((t) => (
            <button
              key={t}
              type="button"
              role="tab"
              aria-selected={tab === t}
              onClick={() => setTab(t)}
              data-testid={`plugins-tab-${t}`}
              className={`-mb-px min-h-9 max-md:min-h-11 cursor-pointer border-b-2 text-[13px] max-md:text-[15px] capitalize transition-colors ${
                tab === t
                  ? "border-brand-500 font-medium text-text-primary"
                  : "border-transparent text-text-dim hover:text-text-secondary"
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
          {error && (
            <PanelMessage tone="error" className="px-1">
              {error}
            </PanelMessage>
          )}

          {data && data.load_errors.length > 0 && (
            <Notice tone="warning" title="Plugin load problems">
              {data.load_errors.map((e) => (
                <p key={e}>{e}</p>
              ))}
            </Notice>
          )}

          {!readOnly && (
            <button
              type="button"
              className={BUTTON_SECONDARY}
              disabled={updates.checking}
              onClick={() => void updates.check()}
              data-testid="plugins-check-updates"
            >
              {updates.checking ? "Checking…" : "Check for updates"}
            </button>
          )}

          {data && data.plugins.length === 0 && (
            <PanelMessage className="px-1" testId="plugins-empty">
              No plugins detected.
            </PanelMessage>
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
              onToggle={(enabled) =>
                plugin.id === WEB_PLUGIN_ID && !enabled ? setConfirmWebOff(plugin) : void onToggle(plugin, enabled)
              }
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

      {confirmWebOff && (
        <WebDashboardOffConfirm
          onCancel={() => setConfirmWebOff(null)}
          onConfirm={() => {
            setConfirmWebOff(null);
            void onToggle(confirmWebOff, false);
          }}
        />
      )}

      {confirmUninstall && (
        <div
          ref={uninstallDialogRef}
          className={`fixed inset-0 z-50 flex items-center justify-center ${BACKDROP} p-4`}
          role="dialog"
          aria-modal="true"
          aria-label={`Uninstall ${confirmUninstall.name}`}
          onClick={() => setConfirmUninstall(null)}
          data-testid="plugin-uninstall-confirm"
        >
          <div className={`w-full max-w-sm ${DIALOG} p-5 text-sm`} onClick={(e) => e.stopPropagation()}>
            <h2 className="mb-2 text-[15px] font-semibold text-text-primary">Uninstall {confirmUninstall.name}?</h2>
            <p className="mb-4 text-[13px] leading-normal text-text-secondary">
              This removes the plugin's files, config entry, and lockfile entry from the host. You can reinstall it
              later from the marketplace.
            </p>
            <div className="flex justify-end gap-2">
              <button
                type="button"
                className={BUTTON_GHOST}
                onClick={() => setConfirmUninstall(null)}
                data-testid="plugin-uninstall-cancel"
              >
                Cancel
              </button>
              <button
                type="button"
                className={BUTTON_DANGER}
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

/** Disabling the Web dashboard plugin turns off the page the user is on once the Daemon restarts. */
function WebDashboardOffConfirm({ onConfirm, onCancel }: { onConfirm: () => void; onCancel: () => void }) {
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  // A ref, not state, so an Enter+click double fire is blocked before a re-render.
  const firedRef = useRef(false);
  const confirm = useCallback(() => {
    if (firedRef.current) return;
    firedRef.current = true;
    onConfirm();
  }, [onConfirm]);
  useDialogFocus(confirmButtonRef);
  useConfirmKeys(onCancel, confirm);

  return (
    <Dialog
      id="web-dashboard-off-dialog"
      title="Turn off the Web dashboard?"
      onDismiss={onCancel}
      footer={
        <>
          <CancelButton onClick={onCancel} />
          <ConfirmButton
            buttonRef={confirmButtonRef}
            onClick={confirm}
            testId="web-dashboard-off-confirm"
            tone="danger"
          >
            Turn off
          </ConfirmButton>
        </>
      }
    >
      <p className="text-[13px] text-text-secondary">
        It keeps running until the Daemon restarts, then <code>aoe serve</code> won&apos;t start until you turn it back
        on.
      </p>
    </Dialog>
  );
}

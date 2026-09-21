import type { PluginUpdateStatus, PluginView } from "../../lib/api";
import { PluginIdentityIcon } from "./PluginIdentityIcon";
import { uniqueSlots } from "./uniqueSlots";

interface Props {
  plugin: PluginView;
  update: PluginUpdateStatus | undefined;
  readOnly: boolean;
  toggleBusy: boolean;
  updating: boolean;
  onOpen: () => void;
  onToggle: (enabled: boolean) => void;
  onUpdate: () => void;
  onUninstall: () => void;
}

const PILL = "rounded px-1.5 py-0.5 text-[10px] uppercase tracking-wide";

/** One installed plugin: identity, badges, access, update state, and controls. */
export function PluginCard({
  plugin,
  update,
  readOnly,
  toggleBusy,
  updating,
  onOpen,
  onToggle,
  onUpdate,
  onUninstall,
}: Props) {
  const ui = plugin.ui_contributions ?? [];
  return (
    <div className="rounded border border-surface-700 bg-surface-850 p-3" data-testid={`plugin-${plugin.id}`}>
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <PluginIdentityIcon
              icon={plugin.icon}
              iconAssetUrl={plugin.icon_asset_url}
              testId={`plugin-icon-${plugin.id}`}
            />
            <button
              type="button"
              className="font-medium hover:underline"
              onClick={onOpen}
              data-testid={`plugin-open-${plugin.id}`}
            >
              {plugin.name}
            </button>
            <span className="text-xs text-text-dim">v{plugin.version}</span>
            <span className={`${PILL} bg-accent-500/20 text-accent-500`} data-testid={`plugin-validation-${plugin.id}`}>
              {plugin.validation}
            </span>
            {plugin.needs_reapproval && (
              <span
                className={`${PILL} bg-status-warning/20 text-status-warning`}
                data-testid={`plugin-needs-approval-${plugin.id}`}
              >
                needs approval
              </span>
            )}
            {update?.needs_update && (
              <span
                className={`${PILL} bg-accent-500/20 text-accent-500`}
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
          {ui.length > 0 && <p className="mt-1 text-[11px] text-text-dim">UI: {uniqueSlots(ui)}</p>}
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
              {!readOnly && (
                <button
                  type="button"
                  className="rounded border border-surface-700 px-2 py-0.5 text-[11px] hover:bg-surface-800 disabled:opacity-50"
                  disabled={updating}
                  onClick={onUpdate}
                  data-testid={`plugin-update-${plugin.id}`}
                >
                  {updating ? "Checking…" : "Update"}
                </button>
              )}
            </div>
          )}
          {update?.error && <p className="mt-1 text-[11px] text-status-error">Update check failed: {update.error}</p>}
        </div>
        {!readOnly && (
          <div className="flex shrink-0 flex-col items-end gap-2">
            <label className="flex items-center gap-1 text-xs">
              <input
                type="checkbox"
                role="switch"
                aria-label={`Enable ${plugin.name}`}
                checked={plugin.enabled}
                disabled={toggleBusy}
                onChange={(e) => onToggle(e.target.checked)}
              />
              Enabled
            </label>
            {!plugin.builtin && plugin.source && (
              <button
                type="button"
                className="rounded border border-status-error/50 px-2 py-0.5 text-[11px] text-status-error hover:bg-status-error/10 disabled:opacity-50"
                onClick={onUninstall}
                data-testid={`plugin-uninstall-${plugin.id}`}
              >
                Uninstall
              </button>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

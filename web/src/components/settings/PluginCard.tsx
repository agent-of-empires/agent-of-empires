import type { PluginUpdateStatus, PluginView } from "../../lib/api";
import { CARD } from "../../lib/surfaces";
import { BUTTON_DANGER_GHOST_SM, BUTTON_SECONDARY_SM } from "../../lib/surfaces";
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

/** A plugin's short status badge. */
export const PILL = "rounded px-1.5 py-0.5 font-mono text-[11px] leading-4";

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
    <div className={`${CARD} py-3`} data-testid={`plugin-${plugin.id}`}>
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
              className="cursor-pointer text-[15px] md:text-sm font-medium text-text-primary hover:underline"
              onClick={onOpen}
              data-testid={`plugin-open-${plugin.id}`}
            >
              {plugin.name}
            </button>
            <span className="font-mono text-[12px] text-text-dim">v{plugin.version}</span>
            <span
              className={`${PILL} ${plugin.validation === "dev" ? "bg-status-warning/20 text-status-warning" : "bg-accent-500/20 text-accent-500"}`}
              data-testid={`plugin-validation-${plugin.id}`}
            >
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
          <p className="mt-1 text-[13px] leading-normal text-text-secondary">{plugin.description}</p>
          {plugin.capabilities.length > 0 && (
            <p className="mt-1 text-[12px] text-text-dim">
              Capabilities: {plugin.capabilities.join(", ")}
              {plugin.granted ? "" : " (not granted)"}
            </p>
          )}
          {ui.length > 0 && <p className="mt-1 text-[12px] text-text-dim">UI: {uniqueSlots(ui)}</p>}
          {plugin.needs_reapproval && (
            <p className="mt-1 text-[12px] text-status-warning">
              Installed but inactive. Re-approve with <code>aoe plugin update {plugin.id}</code>.
            </p>
          )}
          {update?.needs_update && (
            <div className="mt-1 flex flex-wrap items-center gap-2">
              <span className="text-[12px] text-text-dim">
                Update available ({update.current} → {update.available ?? "modified"}).
              </span>
              {!readOnly && (
                <button
                  type="button"
                  className={BUTTON_SECONDARY_SM}
                  disabled={updating}
                  onClick={onUpdate}
                  data-testid={`plugin-update-${plugin.id}`}
                >
                  {updating ? "Checking…" : "Update"}
                </button>
              )}
            </div>
          )}
          {update?.error && (
            <p className="mt-1 text-[12px] text-status-error-text">Update check failed: {update.error}</p>
          )}
        </div>
        {!readOnly && (
          <div className="flex shrink-0 flex-col items-end gap-2">
            <label className="flex min-h-7 max-md:min-h-11 cursor-pointer items-center gap-1.5 text-[13px] text-text-secondary">
              <input
                type="checkbox"
                role="switch"
                aria-label={`Enable ${plugin.name}`}
                checked={plugin.enabled}
                disabled={toggleBusy}
                onChange={(e) => onToggle(e.target.checked)}
                className="accent-brand-600 w-4 h-4"
              />
              Enabled
            </label>
            {!plugin.builtin && plugin.source && (
              <button
                type="button"
                className={BUTTON_DANGER_GHOST_SM}
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

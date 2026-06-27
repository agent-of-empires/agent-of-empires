import { useEffect } from "react";

import type { PluginUpdateConsent } from "../../lib/api";

interface PluginUpdateConsentModalProps {
  /** The structured disclosure for the available update. */
  consent: PluginUpdateConsent;
  /** Plugin display name for the header. */
  name: string;
  /** True while an apply/dismiss request is in flight. */
  busy: boolean;
  /** Inline error from the last apply/dismiss attempt, if any. */
  error: string | null;
  /** Approve the expanded access and apply the update. */
  onApprove: () => void;
  /** Decline: keep the current version and stop nagging until the next version. */
  onDecline: () => void;
  /** Close without recording a decision (Esc / backdrop / Close button). */
  onClose: () => void;
}

/// The in-app capability-consent popup for a plugin update that expands access.
/// Renders the same disclosure the terminal prompt prints (capability diff, UI
/// slots, build commands, runtime / trust changes) and gates the update behind
/// an explicit Approve. Declining keeps the active version and records the
/// dismissal; closing makes no decision.
export function PluginUpdateConsentModal({
  consent,
  name,
  busy,
  error,
  onApprove,
  onDecline,
  onClose,
}: PluginUpdateConsentModalProps) {
  // While an apply/dismiss is in flight, the modal must not close: dropping it
  // would re-expose the Update button and let the same flow start concurrently.
  const closeIfIdle = () => {
    if (!busy) onClose();
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!busy && e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={`Approve update for ${name}`}
      onClick={closeIfIdle}
      data-testid="plugin-update-consent-modal"
    >
      <div
        className="max-h-[80vh] w-full max-w-lg overflow-auto rounded border border-surface-700 bg-surface-900 p-4 text-sm"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-start justify-between gap-3">
          <div>
            <h2 className="font-semibold">Update {name}?</h2>
            <p className="text-xs text-text-dim">
              v{consent.from_version} → v{consent.to_version}
            </p>
          </div>
          <button
            type="button"
            className="rounded border border-surface-700 px-2 py-0.5 text-xs hover:bg-surface-800 disabled:opacity-50"
            disabled={busy}
            onClick={closeIfIdle}
            data-testid="plugin-update-consent-close"
          >
            Close
          </button>
        </div>

        <p className="mb-3 text-xs text-text-dim">
          This update expands what the plugin can do. Review the new access before approving.
        </p>

        {consent.added_capabilities.length > 0 && (
          <div className="mb-3" data-testid="plugin-update-added-caps">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-status-warning">
              New capabilities
            </p>
            <p className="text-xs text-status-warning">{consent.added_capabilities.join(", ")}</p>
          </div>
        )}

        {consent.removed_capabilities.length > 0 && (
          <div className="mb-3">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-text-dim">Removed capabilities</p>
            <p className="text-xs text-text-dim">{consent.removed_capabilities.join(", ")}</p>
          </div>
        )}

        {consent.runtime_change && (
          <p className="mb-3 text-xs text-status-warning" data-testid="plugin-update-runtime-change">
            Runtime change: {consent.runtime_change}
          </p>
        )}

        {consent.trust_downgrade && (
          <p className="mb-3 text-xs text-status-warning" data-testid="plugin-update-trust-downgrade">
            This version is no longer a verified featured plugin (community trust).
          </p>
        )}

        {consent.build_steps.length > 0 && (
          <div className="mb-3" data-testid="plugin-update-build-steps">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-status-warning">
              Build commands (run as you, unsandboxed)
            </p>
            <ul className="space-y-0.5">
              {consent.build_steps.map((step, i) => (
                <li key={i} className="font-mono text-[11px] text-text-dim">
                  $ {step}
                </li>
              ))}
            </ul>
          </div>
        )}

        {consent.ui.length > 0 && (
          <div className="mb-3">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-text-dim">Dashboard UI slots</p>
            <p className="text-xs text-text-dim">{[...new Set(consent.ui.map((u) => u.slot))].join(", ")}</p>
          </div>
        )}

        <p className="mb-3 text-[11px] text-text-dim">
          Approving trusts this plugin. The host enforces capabilities at its API boundary, but a plugin worker (and any
          build step) runs without OS-level sandboxing, so a malicious plugin is not contained. Only approve updates
          from sources you trust.
        </p>

        {error && (
          <p className="mb-3 text-xs text-status-error" data-testid="plugin-update-consent-error">
            {error}
          </p>
        )}

        <div className="flex justify-end gap-2">
          <button
            type="button"
            className="rounded border border-surface-700 px-3 py-1 text-xs hover:bg-surface-800 disabled:opacity-50"
            disabled={busy}
            onClick={onDecline}
            data-testid="plugin-update-decline"
          >
            Decline
          </button>
          <button
            type="button"
            className="rounded bg-brand-600 px-3 py-1 text-xs font-medium text-white hover:bg-brand-500 disabled:opacity-50"
            disabled={busy}
            onClick={onApprove}
            data-testid="plugin-update-approve"
          >
            {busy ? "Updating…" : "Approve and update"}
          </button>
        </div>
      </div>
    </div>
  );
}

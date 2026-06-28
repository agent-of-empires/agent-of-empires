import { useEffect } from "react";

import type { PluginInstallConsent } from "../../lib/api";

interface PluginInstallConsentModalProps {
  /** The structured disclosure for the install candidate. */
  consent: PluginInstallConsent;
  /** True while the install request is being started. */
  busy: boolean;
  /** Inline error from the last start attempt, if any. */
  error: string | null;
  /** Approve the disclosed access and start the install. */
  onApprove: () => void;
  /** Close without installing (Esc / backdrop / Cancel button). */
  onClose: () => void;
}

/// The in-app capability-consent popup for a web plugin install. Renders the
/// same disclosure the terminal prompt prints (capabilities, build commands, UI
/// slots, unverified-source warning) and gates the install behind an explicit
/// Approve, so the web path never silently grants what the CLI prompts for.
export function PluginInstallConsentModal({
  consent,
  busy,
  error,
  onApprove,
  onClose,
}: PluginInstallConsentModalProps) {
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
      aria-label={`Install ${consent.id}`}
      onClick={closeIfIdle}
      data-testid="plugin-install-consent-modal"
    >
      <div
        className="max-h-[80vh] w-full max-w-lg overflow-auto rounded border border-surface-700 bg-surface-900 p-4 text-sm"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-start justify-between gap-3">
          <div>
            <h2 className="font-semibold">Install {consent.id}?</h2>
            <p className="text-xs text-text-dim">
              v{consent.version} ·{" "}
              <span className={consent.validation === "featured" ? "font-medium text-accent-500" : undefined}>
                {consent.validation}
              </span>{" "}
              · {consent.source}
            </p>
          </div>
          <button
            type="button"
            className="rounded border border-surface-700 px-2 py-0.5 text-xs hover:bg-surface-800 disabled:opacity-50"
            disabled={busy}
            onClick={closeIfIdle}
            data-testid="plugin-install-consent-close"
          >
            Close
          </button>
        </div>

        <p className="mb-3 text-xs text-text-dim">{consent.notice}</p>

        {consent.unverified && (
          <p className="mb-3 text-xs text-status-warning" data-testid="plugin-install-unverified">
            This is unverified, un-audited code: it does not come from a vetted release and is not covered by the
            featured index. Install it only if you trust the source.
          </p>
        )}

        {consent.capabilities.length > 0 && (
          <div className="mb-3" data-testid="plugin-install-caps">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-status-warning">Capabilities</p>
            <p className="text-xs text-status-warning">{consent.capabilities.join(", ")}</p>
          </div>
        )}

        {consent.build_steps.length > 0 && (
          <div className="mb-3" data-testid="plugin-install-build-steps">
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
          Installing trusts this plugin. The host enforces capabilities at its API boundary, but a plugin worker (and
          any build step) runs without OS-level sandboxing, so a malicious plugin is not contained. Build steps run as
          you before any capability gate. Only install plugins you trust.
        </p>

        {error && (
          <p className="mb-3 text-xs text-status-error" data-testid="plugin-install-consent-error">
            {error}
          </p>
        )}

        <div className="flex justify-end gap-2">
          <button
            type="button"
            className="rounded border border-surface-700 px-3 py-1 text-xs hover:bg-surface-800 disabled:opacity-50"
            disabled={busy}
            onClick={closeIfIdle}
            data-testid="plugin-install-cancel"
          >
            Cancel
          </button>
          <button
            type="button"
            className="rounded bg-brand-600 px-3 py-1 text-xs font-medium text-white hover:bg-brand-500 disabled:opacity-50"
            disabled={busy}
            onClick={onApprove}
            data-testid="plugin-install-approve"
          >
            {busy ? "Starting…" : "Approve and install"}
          </button>
        </div>
      </div>
    </div>
  );
}

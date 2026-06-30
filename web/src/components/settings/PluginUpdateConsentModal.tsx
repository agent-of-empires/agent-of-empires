import { useEffect } from "react";

import type { PluginUpdateChangelog, PluginUpdateConsent } from "../../lib/api";

interface PluginUpdateConsentModalProps {
  /** The access disclosure when the update expands what the plugin can do, or
   *  null for a safe version bump (changelog only, no consent). */
  consent: PluginUpdateConsent | null;
  /** Plugin display name for the header. */
  name: string;
  /** Installed version, for the v{from} -> v{to} header. */
  fromVersion: string;
  /** Target version. */
  toVersion: string;
  /** What changed between the two versions. */
  changelog: PluginUpdateChangelog;
  /** True while an apply/dismiss request is in flight. */
  busy: boolean;
  /** Inline error from the last apply/dismiss attempt, if any. */
  error: string | null;
  /** Apply the update (safe: "Update"; consent: "Approve and update"). */
  onApprove: () => void;
  /** Consent mode only: decline and stop nagging until the next version. Absent
   *  for a safe update, which has nothing to dismiss. */
  onDecline?: () => void;
  /** Close without recording a decision (Esc / backdrop / Close / Cancel). */
  onClose: () => void;
}

/// The in-app update review popup, used for every in-UI plugin update. It always
/// shows the changelog between the installed and target version. When the update
/// also expands access (`consent` is non-null) it adds the capability diff, UI
/// slots, build commands, and runtime / trust disclosures, and gates behind an
/// explicit Approve with a Decline that records the dismissal. A safe version
/// bump (`consent` is null) shows only the changelog with Cancel / Update.
export function PluginUpdateConsentModal({
  consent,
  name,
  fromVersion,
  toVersion,
  changelog,
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

  const needsConsent = consent !== null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={`${needsConsent ? "Approve update for" : "Update"} ${name}`}
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
              v{fromVersion} → v{toVersion}
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

        <PluginChangelogSection changelog={changelog} />

        {needsConsent && (
          <p className="mb-3 text-xs text-text-dim">
            This update expands what the plugin can do. Review the new access before approving.
          </p>
        )}

        {consent && consent.added_capabilities.length > 0 && (
          <div className="mb-3" data-testid="plugin-update-added-caps">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-status-warning">
              New capabilities
            </p>
            <p className="text-xs text-status-warning">{consent.added_capabilities.join(", ")}</p>
          </div>
        )}

        {consent && consent.removed_capabilities.length > 0 && (
          <div className="mb-3">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-text-dim">Removed capabilities</p>
            <p className="text-xs text-text-dim">{consent.removed_capabilities.join(", ")}</p>
          </div>
        )}

        {consent?.runtime_change && (
          <p className="mb-3 text-xs text-status-warning" data-testid="plugin-update-runtime-change">
            Runtime change: {consent.runtime_change}
          </p>
        )}

        {consent?.trust_downgrade && (
          <p className="mb-3 text-xs text-status-warning" data-testid="plugin-update-trust-downgrade">
            This version is no longer a verified featured plugin (community trust).
          </p>
        )}

        {consent && consent.build_steps.length > 0 && (
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

        {consent && consent.ui.length > 0 && (
          <div className="mb-3">
            <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-text-dim">Dashboard UI slots</p>
            <p className="text-xs text-text-dim">{[...new Set(consent.ui.map((u) => u.slot))].join(", ")}</p>
          </div>
        )}

        {needsConsent && (
          <p className="mb-3 text-[11px] text-text-dim">
            Approving trusts this plugin. The host enforces capabilities at its API boundary, but a plugin worker (and
            any build step) runs without OS-level sandboxing, so a malicious plugin is not contained. Only approve
            updates from sources you trust.
          </p>
        )}

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
            onClick={needsConsent ? onDecline : closeIfIdle}
            data-testid="plugin-update-decline"
          >
            {needsConsent ? "Decline" : "Cancel"}
          </button>
          <button
            type="button"
            className="rounded bg-brand-600 px-3 py-1 text-xs font-medium text-white hover:bg-brand-500 disabled:opacity-50"
            disabled={busy}
            onClick={onApprove}
            data-testid="plugin-update-approve"
          >
            {busy ? "Updating…" : needsConsent ? "Approve and update" : "Update"}
          </button>
        </div>
      </div>
    </div>
  );
}

/// The changelog list between the installed and target version. Release notes
/// render as escaped plain text (React escapes by default; no raw HTML). An
/// unavailable changelog says so rather than looking like "no changes".
function PluginChangelogSection({ changelog }: { changelog: PluginUpdateChangelog }) {
  if (changelog.unavailable_reason) {
    return (
      <p className="mb-3 text-xs text-text-dim" data-testid="plugin-update-changelog-unavailable">
        {changelog.unavailable_reason}
      </p>
    );
  }
  if (changelog.entries.length === 0) {
    return (
      <p className="mb-3 text-xs text-text-dim" data-testid="plugin-update-changelog-empty">
        No changelog available.
      </p>
    );
  }
  return (
    <div className="mb-3" data-testid="plugin-update-changelog">
      <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-text-dim">What's new</p>
      <ul className="space-y-2">
        {changelog.entries.map((entry, i) =>
          entry.kind === "release" ? (
            <li key={`r-${entry.tag}-${i}`} className="text-xs">
              <p className="font-medium text-text">{entry.tag}</p>
              {entry.body && <p className="whitespace-pre-wrap text-text-dim">{entry.body}</p>}
            </li>
          ) : (
            <li key={`c-${entry.sha}-${i}`} className="flex gap-2 text-xs">
              <span className="font-mono text-text-dim">{entry.sha.slice(0, 7)}</span>
              <span className="text-text-dim">{entry.subject}</span>
            </li>
          ),
        )}
      </ul>
      {changelog.truncated && (
        <p className="mt-1 text-[11px] text-text-dim" data-testid="plugin-update-changelog-truncated">
          Showing the most recent entries.{" "}
          {changelog.more_url && (
            <a
              href={changelog.more_url}
              target="_blank"
              rel="noreferrer"
              className="text-brand-400 underline hover:text-brand-300"
              data-testid="plugin-update-changelog-more"
            >
              View the full changelog on GitHub
            </a>
          )}
        </p>
      )}
    </div>
  );
}

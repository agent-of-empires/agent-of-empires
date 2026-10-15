import type { PluginUpdateChangelog, PluginUpdateConsent } from "../../lib/api";
import { BuildSteps, ModalActions, ModalSection, PluginModal } from "./PluginModal";
import { uniqueSlots } from "./uniqueSlots";

interface Props {
  /** The access disclosure, or null for a safe version bump (changelog only). */
  consent: PluginUpdateConsent | null;
  name: string;
  fromVersion: string;
  toVersion: string;
  changelog: PluginUpdateChangelog;
  busy: boolean;
  error: string | null;
  onApprove: () => void;
  /** Consent mode only: records the dismissal. */
  onDecline?: () => void;
  onClose: () => void;
}

/** Update review: always the changelog, plus the access diff when the update expands access. */
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
}: Props) {
  const needsConsent = consent !== null;
  // Closing mid-request would re-expose Update and allow a concurrent flow.
  const closeIfIdle = () => {
    if (!busy) onClose();
  };

  return (
    <PluginModal
      testId="plugin-update-consent-modal"
      ariaLabel={`${needsConsent ? "Approve update for" : "Update"} ${name}`}
      closeTestId="plugin-update-consent-close"
      busy={busy}
      onClose={onClose}
      header={
        <div>
          <h2 className="font-semibold">Update {name}?</h2>
          <p className="text-xs text-text-dim">
            v{fromVersion} → v{toVersion}
          </p>
        </div>
      }
    >
      <ChangelogSection changelog={changelog} />

      {needsConsent && (
        <p className="mb-3 text-xs text-text-dim">
          This update expands what the plugin can do. Review the new access before approving.
        </p>
      )}

      {consent && consent.added_capabilities.length > 0 && (
        <ModalSection label="New capabilities" warn testId="plugin-update-added-caps">
          {consent.added_capabilities.join(", ")}
        </ModalSection>
      )}

      {consent && consent.removed_capabilities.length > 0 && (
        <ModalSection label="Removed capabilities">{consent.removed_capabilities.join(", ")}</ModalSection>
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

      {consent && <BuildSteps steps={consent.build_steps} testId="plugin-update-build-steps" />}

      {consent && consent.ui.length > 0 && (
        <ModalSection label="Dashboard UI slots">{uniqueSlots(consent.ui)}</ModalSection>
      )}

      {needsConsent && (
        <p className="mb-3 text-[11px] text-text-dim">
          Approving trusts this plugin. The host enforces capabilities at its API boundary, but a plugin worker (and any
          build step) runs without OS-level sandboxing, so a malicious plugin is not contained. Only approve updates
          from sources you trust.
        </p>
      )}

      {error && (
        <p className="mb-3 text-xs text-status-error" data-testid="plugin-update-consent-error">
          {error}
        </p>
      )}

      <ModalActions
        busy={busy}
        cancelLabel={needsConsent ? "Decline" : "Cancel"}
        cancelTestId="plugin-update-decline"
        onCancel={() => (needsConsent ? onDecline?.() : closeIfIdle())}
        confirmLabel={busy ? "Updating…" : needsConsent ? "Approve and update" : "Update"}
        confirmTestId="plugin-update-approve"
        onConfirm={onApprove}
      />
    </PluginModal>
  );
}

/** Release notes render as escaped text; an unavailable changelog must not read as "no changes". */
function ChangelogSection({ changelog }: { changelog: PluginUpdateChangelog }) {
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
    <ModalSection label="What's new" testId="plugin-update-changelog">
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
    </ModalSection>
  );
}

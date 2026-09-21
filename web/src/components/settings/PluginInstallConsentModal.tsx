import type { PluginInstallConsent } from "../../lib/api";
import { BuildSteps, ModalActions, ModalSection, PluginModal } from "./PluginModal";
import { uniqueSlots } from "./uniqueSlots";

interface Props {
  consent: PluginInstallConsent;
  busy: boolean;
  error: string | null;
  onApprove: () => void;
  onClose: () => void;
}

/** Capability consent for a web install, matching the CLI prompt's disclosure. */
export function PluginInstallConsentModal({ consent, busy, error, onApprove, onClose }: Props) {
  return (
    <PluginModal
      testId="plugin-install-consent-modal"
      ariaLabel={`Install ${consent.id}`}
      closeTestId="plugin-install-consent-close"
      busy={busy}
      onClose={onClose}
      header={
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
      }
    >
      <p className="mb-3 text-xs text-text-dim">{consent.notice}</p>

      {consent.unverified && (
        <p className="mb-3 text-xs text-status-warning" data-testid="plugin-install-unverified">
          This is unverified, un-audited code: it does not come from a vetted release and is not covered by the featured
          index. Install it only if you trust the source.
        </p>
      )}

      {consent.capabilities.length > 0 && (
        <ModalSection label="Capabilities" warn testId="plugin-install-caps">
          {consent.capabilities.join(", ")}
        </ModalSection>
      )}

      <BuildSteps steps={consent.build_steps} testId="plugin-install-build-steps" />

      {consent.ui.length > 0 && <ModalSection label="Dashboard UI slots">{uniqueSlots(consent.ui)}</ModalSection>}

      <p className="mb-3 text-[11px] text-text-dim">
        Installing trusts this plugin. The host enforces capabilities at its API boundary, but a plugin worker (and any
        build step) runs without OS-level sandboxing, so a malicious plugin is not contained. Build steps run as you
        before any capability gate. Only install plugins you trust.
      </p>

      {error && (
        <p className="mb-3 text-xs text-status-error" data-testid="plugin-install-consent-error">
          {error}
        </p>
      )}

      <ModalActions
        busy={busy}
        cancelLabel="Cancel"
        cancelTestId="plugin-install-cancel"
        onCancel={onClose}
        confirmLabel={busy ? "Starting…" : "Approve and install"}
        confirmTestId="plugin-install-approve"
        onConfirm={onApprove}
      />
    </PluginModal>
  );
}

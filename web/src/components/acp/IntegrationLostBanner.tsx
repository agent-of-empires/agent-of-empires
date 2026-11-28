import type { IntegrationLostReason } from "../../lib/acpTypes";

/** What went wrong, per reason; the Daemon holds the session's queue meanwhile. */
function integrationLostLine(reason: IntegrationLostReason): string {
  switch (reason.kind) {
    case "contract_violation":
      return `The integration broke the protocol: ${reason.detail}.`;
    case "hold_overflow":
      return "The integration held back too many messages.";
    case "lease_overflow":
      return "The integration left too many requests open.";
    case "proxy_exited":
      return "The integration process exited.";
  }
}

/** The Agent's plugin integration failed mid-session. The session itself keeps running. */
export function IntegrationLostBanner({ reason }: { reason: IntegrationLostReason }) {
  return (
    <section
      aria-label="Agent integration lost"
      data-testid="integration-lost-banner"
      className="border-b border-status-error/40 bg-status-error/10 px-4 py-3 text-text-primary"
    >
      <div role="alert">
        <h2 className="text-sm font-semibold">Agent integration lost</h2>
        <p data-testid="integration-lost-line" className="mt-1 text-xs text-text-subtle">
          {integrationLostLine(reason)} Messages you send stay queued.
        </p>
      </div>
    </section>
  );
}

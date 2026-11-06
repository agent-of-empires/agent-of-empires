// @vitest-environment jsdom
import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { PluginAgentIntegration, PluginInstallConsent, PluginUpdateConsent } from "../../../lib/api";
import { PluginInstallConsentModal } from "../PluginInstallConsentModal";
import { PluginUpdateConsentModal } from "../PluginUpdateConsentModal";

const integration: PluginAgentIntegration = {
  tool: "acme-helper",
  command: "ACME_TOKEN=local acme-helper --serve",
  provider_versions: ["^0.2"],
  replies: false,
  launch: {
    oneshot_flag: "-p",
    container_env: { ACME_HOME: "/state" },
  },
};

const installConsent: PluginInstallConsent = {
  id: "acme.declared",
  version: "1.2.0",
  source: "file:///acme",
  notice: "Installs the Acme helper.",
  unverified: false,
  validation: "community",
  capabilities: ["session.integration"],
  ui: [],
  agent_integrations: [integration],
  build_steps: [],
  fingerprint: "abc",
};

const updateConsent: PluginUpdateConsent = {
  id: "acme.declared",
  from_version: "1.1.0",
  to_version: "1.2.0",
  prior_capabilities: [],
  new_capabilities: ["session.integration"],
  added_capabilities: ["session.integration"],
  removed_capabilities: [],
  ui: [],
  agent_integrations: [integration],
  build_steps: [],
  runtime_change: null,
  trust_downgrade: false,
  fingerprint: "abc",
  stays_active_if_declined: true,
  changelog: { entries: [], truncated: false, unavailable_reason: null, more_url: null },
};

const noop = () => {};

describe("plugin consent modals", () => {
  it("renders a populated declaration in the install consent disclosure", () => {
    const { getByTestId, getByText } = render(
      <PluginInstallConsentModal consent={installConsent} busy={false} error={null} onApprove={noop} onClose={noop} />,
    );
    expect(getByTestId("plugin-install-agent-integrations").textContent).toContain(
      "ACME_TOKEN=local acme-helper --serve",
    );
    expect(getByText("sandbox environment: ACME_HOME=/state")).toBeTruthy();
    expect(getByText("provider versions: ^0.2")).toBeTruthy();
    expect(getByText("native replies: no")).toBeTruthy();
  });

  it("renders the same declaration in the update consent disclosure", () => {
    const { getByTestId, getByText } = render(
      <PluginUpdateConsentModal
        consent={updateConsent}
        name="Acme"
        fromVersion="1.1.0"
        toVersion="1.2.0"
        changelog={updateConsent.changelog}
        busy={false}
        error={null}
        onApprove={noop}
        onClose={noop}
      />,
    );
    expect(getByTestId("plugin-update-agent-integrations").textContent).toContain(
      "ACME_TOKEN=local acme-helper --serve",
    );
    expect(getByText("provider versions: ^0.2")).toBeTruthy();
    expect(getByText("native replies: no")).toBeTruthy();
  });
});

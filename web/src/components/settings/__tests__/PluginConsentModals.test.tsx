// @vitest-environment jsdom
import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { PluginAgentIntegration, PluginInstallConsent, PluginUpdateConsent } from "../../../lib/api";
import { PluginInstallConsentModal } from "../PluginInstallConsentModal";
import { PluginUpdateConsentModal } from "../PluginUpdateConsentModal";

const integration: PluginAgentIntegration = {
  tool: "acme\u202ehelper",
  command: "agent --run",
  provider_versions: ["^1"],
  replies: false,
  launch: { container_env: { ACME_HOME: "/state\u001b[2J" }, oneshot_flag: "-p" },
  detection: { mode: "native_only" },
  reply_fallback: { allow: [{ kind: "literal", value: "y" }], deny: [{ kind: "literal", value: "n" }] },
};
const install: PluginInstallConsent = {
  id: "acme.plugin",
  version: "1.0.0",
  source: "gh:acme/plugin",
  notice: "notice",
  unverified: false,
  validation: "community",
  capabilities: [],
  ui: [],
  agent_integrations: [integration],
  build_steps: [],
  fingerprint: "abc",
};
const update: PluginUpdateConsent = {
  id: "acme.plugin",
  from_version: "0.9.0",
  to_version: "1.0.0",
  prior_capabilities: [],
  new_capabilities: [],
  added_capabilities: [],
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

describe("plugin agent consent", () => {
  it.each(["install", "update"] as const)("renders safe declaration details for %s", (mode) => {
    const view =
      mode === "install"
        ? render(
            <PluginInstallConsentModal consent={install} busy={false} error={null} onApprove={noop} onClose={noop} />,
          )
        : render(
            <PluginUpdateConsentModal
              consent={update}
              name="Acme"
              fromVersion="0.9.0"
              toVersion="1.0.0"
              changelog={update.changelog}
              busy={false}
              error={null}
              onApprove={noop}
              onClose={noop}
            />,
          );
    const text = view.getByTestId(`plugin-${mode}-agent-integrations`).textContent ?? "";
    expect(text).toContain("acme\\u{202e}helper: agent --run");
    expect(text).toContain("sandbox environment: ACME_HOME=/state\\u{1b}[2J");
    expect(text).toContain("detection: native_only");
    expect(text).toContain('reply fallback: allow ["y"], deny ["n"]');
    expect(text).not.toContain("\u202e");
    expect(text).not.toContain("\u001b");
  });
});

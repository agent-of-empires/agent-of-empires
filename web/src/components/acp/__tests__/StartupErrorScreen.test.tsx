// @vitest-environment jsdom
//
// Smoke-coverage for the dedicated startup-error screen. The screen
// only renders when the per-adapter compatibility check rejects the
// adapter; we exercise each variant so a future schema change to
// `IncompatibleAgentDetail` surfaces here loudly.

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";

import { StartupErrorScreen } from "../StartupErrorScreen";

afterEach(() => {
  cleanup();
});

describe("StartupErrorScreen", () => {
  it("renders incompatible_agent_version with installed/required + install command", () => {
    const { container, getByTestId } = render(
      <StartupErrorScreen
        detail={{
          kind: "incompatible_agent_version",
          package_name: "@agentclientprotocol/claude-agent-acp",
          installed: "0.32.0",
          required: "0.39.0",
          install_command:
            "npm install -g @agentclientprotocol/claude-agent-acp@latest",
        }}
      />,
    );
    expect(container.textContent).toContain("0.32.0");
    expect(container.textContent).toContain("0.39.0");
    expect(container.textContent).toContain(
      "@agentclientprotocol/claude-agent-acp",
    );
    const cmd = getByTestId("startup-error-install-command");
    expect(cmd.textContent).toContain(
      "npm install -g @agentclientprotocol/claude-agent-acp@latest",
    );
  });

  it("renders missing_agent_info with the expected package", () => {
    const { container } = render(
      <StartupErrorScreen
        detail={{
          kind: "missing_agent_info",
          expected_package: "@agentclientprotocol/claude-agent-acp",
          install_command:
            "npm install -g @agentclientprotocol/claude-agent-acp@latest",
        }}
      />,
    );
    expect(container.textContent).toContain("did not report its package version");
    expect(container.textContent).toContain(
      "@agentclientprotocol/claude-agent-acp",
    );
  });

  it("renders mismatched_agent_name with both expected and received", () => {
    const { container } = render(
      <StartupErrorScreen
        detail={{
          kind: "mismatched_agent_name",
          expected: "@agentclientprotocol/claude-agent-acp",
          received: "some-wrapper-script",
          install_command:
            "npm install -g @agentclientprotocol/claude-agent-acp@latest",
        }}
      />,
    );
    expect(container.textContent).toContain(
      "@agentclientprotocol/claude-agent-acp",
    );
    expect(container.textContent).toContain("some-wrapper-script");
  });

  it("renders unparseable_agent_version with the raw version string", () => {
    const { container } = render(
      <StartupErrorScreen
        detail={{
          kind: "unparseable_agent_version",
          package_name: "@agentclientprotocol/claude-agent-acp",
          raw_version: "not-semver",
          required: "0.39.0",
          install_command:
            "npm install -g @agentclientprotocol/claude-agent-acp@latest",
        }}
      />,
    );
    expect(container.textContent).toContain("not-semver");
    expect(container.textContent).toContain("0.39.0");
  });

  it("renders unsupported_protocol_version without an install command", () => {
    const { container, queryByTestId } = render(
      <StartupErrorScreen
        detail={{
          kind: "unsupported_protocol_version",
          expected: "V1",
          received: "V2",
        }}
      />,
    );
    expect(container.textContent).toContain("ACP protocol");
    expect(container.textContent).toContain("V1");
    expect(container.textContent).toContain("V2");
    expect(queryByTestId("startup-error-install-command")).toBeNull();
  });
});

// @vitest-environment jsdom
//
// Smoke-coverage for the dedicated startup-error screen. The screen
// only renders when the per-adapter compatibility check rejects the
// adapter; we exercise each variant so a future schema change to
// `IncompatibleAgentDetail` surfaces here loudly, plus the in-UI
// recovery controls (Restart agent / Update & restart). See #2109.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { StartupErrorScreen } from "../StartupErrorScreen";

const fetchSettings = vi.fn();
const installAcpAgent = vi.fn();
vi.mock("../../../lib/api", () => ({
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  installAcpAgent: (...args: unknown[]) => installAcpAgent(...args),
}));

const incompatible = (auto_install = true) => ({
  kind: "incompatible_agent_version" as const,
  package_name: "@agentclientprotocol/claude-agent-acp",
  installed: "0.32.0",
  required: "0.39.0",
  install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
  auto_install,
});

beforeEach(() => {
  fetchSettings.mockReset();
  fetchSettings.mockResolvedValue({});
  installAcpAgent.mockReset();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, text: () => Promise.resolve("") }));
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("StartupErrorScreen", () => {
  it("renders incompatible_agent_version with installed/required + install command", () => {
    const { container, getByTestId } = render(<StartupErrorScreen detail={incompatible()} sessionId="s1" />);
    expect(container.textContent).toContain("0.32.0");
    expect(container.textContent).toContain("0.39.0");
    expect(container.textContent).toContain("@agentclientprotocol/claude-agent-acp");
    const cmd = getByTestId("startup-error-install-command");
    expect(cmd.textContent).toContain("npm install -g @agentclientprotocol/claude-agent-acp@latest");
  });

  it("renders missing_agent_info with the expected package", () => {
    const { container } = render(
      <StartupErrorScreen
        detail={{
          kind: "missing_agent_info",
          expected_package: "@agentclientprotocol/claude-agent-acp",
          install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
          auto_install: true,
        }}
        sessionId="s1"
      />,
    );
    expect(container.textContent).toContain("did not report its package version");
    expect(container.textContent).toContain("@agentclientprotocol/claude-agent-acp");
  });

  it("renders mismatched_agent_name with both expected and received", () => {
    const { container } = render(
      <StartupErrorScreen
        detail={{
          kind: "mismatched_agent_name",
          expected: "@agentclientprotocol/claude-agent-acp",
          received: "some-wrapper-script",
          install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
          auto_install: true,
        }}
        sessionId="s1"
      />,
    );
    expect(container.textContent).toContain("@agentclientprotocol/claude-agent-acp");
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
          install_command: "npm install -g @agentclientprotocol/claude-agent-acp@latest",
          auto_install: true,
        }}
        sessionId="s1"
      />,
    );
    expect(container.textContent).toContain("not-semver");
    expect(container.textContent).toContain("0.39.0");
  });

  it("renders unsupported_protocol_version without an install command", () => {
    const { container, queryByTestId } = render(
      <StartupErrorScreen
        detail={{ kind: "unsupported_protocol_version", expected: "V1", received: "V2" }}
        sessionId="s1"
      />,
    );
    expect(container.textContent).toContain("ACP protocol");
    expect(container.textContent).toContain("V1");
    expect(container.textContent).toContain("V2");
    expect(queryByTestId("startup-error-install-command")).toBeNull();
  });

  it("Restart agent POSTs to /acp/spawn", async () => {
    const { getByTestId } = render(<StartupErrorScreen detail={incompatible()} sessionId="sess%2Fa" />);
    fireEvent.click(getByTestId("startup-error-restart"));
    expect(fetch).toHaveBeenCalledWith(
      "/api/sessions/sess%252Fa/acp/spawn",
      expect.objectContaining({ method: "POST" }),
    );
  });

  it("shows a disabled Update & restart plus an enable hint when the install setting is off", async () => {
    fetchSettings.mockResolvedValue({ acp: { allow_agent_install: false } });
    const { queryByTestId, findByTestId } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" />);
    // The active button stays hidden; the disabled placeholder + hint appear.
    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    expect(queryByTestId("startup-error-update-restart")).toBeNull();
    const disabled = await findByTestId("startup-error-update-restart-disabled");
    expect((disabled as HTMLButtonElement).disabled).toBe(true);
    const hint = await findByTestId("startup-error-enable-hint");
    expect(hint.textContent).toContain("acp.allow_agent_install");
  });

  it("hides Update & restart entirely for non-npm agents even when the setting is on", async () => {
    fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
    const { queryByTestId } = render(<StartupErrorScreen detail={incompatible(false)} sessionId="s1" />);
    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    expect(queryByTestId("startup-error-update-restart")).toBeNull();
    expect(queryByTestId("startup-error-update-restart-disabled")).toBeNull();
    expect(queryByTestId("startup-error-enable-hint")).toBeNull();
  });

  it("Update & restart installs then respawns on success", async () => {
    fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
    installAcpAgent.mockResolvedValue({
      session_id: "s1",
      package: "@agentclientprotocol/claude-agent-acp@latest",
      success: true,
      exit_code: 0,
      stdout: "added 1 package",
      stderr: "",
      recovered_sessions: 0,
    });
    const { findByTestId } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" />);
    const btn = await findByTestId("startup-error-update-restart");
    fireEvent.click(btn);
    expect(installAcpAgent).toHaveBeenCalledWith("s1");
    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith("/api/sessions/s1/acp/spawn", expect.objectContaining({ method: "POST" })),
    );
  });

  it("reports how many other sessions were queued for recovery after a global install", async () => {
    fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
    installAcpAgent.mockResolvedValue({
      session_id: "s1",
      package: "@agentclientprotocol/claude-agent-acp@latest",
      success: true,
      exit_code: 0,
      stdout: "added 1 package",
      stderr: "",
      recovered_sessions: 3,
    });
    const { findByTestId, container } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" />);
    fireEvent.click(await findByTestId("startup-error-update-restart"));
    await waitFor(() => expect(container.textContent).toContain("3 other sessions"));
  });

  it("Update & restart surfaces the error and does not respawn on failure", async () => {
    fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
    installAcpAgent.mockRejectedValue(new Error("npm is not on the daemon's PATH"));
    const { findByTestId, container } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" />);
    const btn = await findByTestId("startup-error-update-restart");
    fireEvent.click(btn);
    await waitFor(() => expect(container.textContent).toContain("npm is not on the daemon's PATH"));
    expect(fetch).not.toHaveBeenCalled();
  });

  it("Update & restart shows the exit code and skips respawn when the install fails", async () => {
    fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
    installAcpAgent.mockResolvedValue({
      session_id: "s1",
      package: "@agentclientprotocol/claude-agent-acp@latest",
      success: false,
      exit_code: 243,
      stdout: "",
      stderr: "npm ERR! EACCES",
    });
    const { findByTestId, container } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" />);
    const btn = await findByTestId("startup-error-update-restart");
    fireEvent.click(btn);
    await waitFor(() => expect(container.textContent).toContain("Install exited with code 243"));
    expect(container.textContent).toContain("npm ERR! EACCES");
    expect(fetch).not.toHaveBeenCalled();
  });

  it("Restart agent surfaces a failed respawn", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 500, text: () => Promise.resolve("boom") }));
    const { getByTestId, container } = render(<StartupErrorScreen detail={incompatible()} sessionId="s1" />);
    fireEvent.click(getByTestId("startup-error-restart"));
    await waitFor(() => expect(container.textContent).toContain("Restart failed"));
  });

  describe("sandboxed session", () => {
    it("hides the host install command and shows the runtime-aware container note", () => {
      const { queryByTestId, getByTestId } = render(
        <StartupErrorScreen detail={incompatible()} sessionId="s1" isSandboxed />,
      );
      // The host copy-paste block is misleading in a sandbox, so it is gone.
      expect(queryByTestId("startup-error-install-command")).toBeNull();
      // The container-aware note replaces it and points at the durable fix.
      const note = getByTestId("startup-error-sandbox-note").textContent ?? "";
      expect(note).toContain("inside the container");
      expect(note).toContain("sandbox image update available");
      // No hardcoded, runtime-specific pull command: a literal `docker pull`
      // would be wrong for Podman / Apple Container or a custom image.
      expect(note).not.toContain("docker pull");
    });

    it("relabels the recovery button to install inside the sandbox", async () => {
      fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
      const { findByTestId } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" isSandboxed />);
      const btn = await findByTestId("startup-error-update-restart");
      expect(btn.textContent).toContain("Update in sandbox & restart");
    });

    it("Update in sandbox & restart calls the same install endpoint then respawns", async () => {
      fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
      installAcpAgent.mockResolvedValue({
        session_id: "s1",
        package: "@agentclientprotocol/claude-agent-acp@latest",
        success: true,
        exit_code: 0,
        stdout: "added 1 package",
        stderr: "",
        recovered_sessions: 0,
      });
      const { findByTestId } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" isSandboxed />);
      fireEvent.click(await findByTestId("startup-error-update-restart"));
      expect(installAcpAgent).toHaveBeenCalledWith("s1");
      await waitFor(() =>
        expect(fetch).toHaveBeenCalledWith("/api/sessions/s1/acp/spawn", expect.objectContaining({ method: "POST" })),
      );
    });

    it("enable hint says the install runs inside the sandbox container", async () => {
      fetchSettings.mockResolvedValue({ acp: { allow_agent_install: false } });
      const { findByTestId } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" isSandboxed />);
      const hint = await findByTestId("startup-error-enable-hint");
      expect(hint.textContent).toContain("inside the sandbox container");
    });
  });
});

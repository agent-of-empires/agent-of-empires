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

const installOk = (recovered_sessions = 0) => ({
  session_id: "s1",
  package: "@agentclientprotocol/claude-agent-acp@latest",
  success: true,
  exit_code: 0,
  stdout: "added 1 package",
  stderr: "",
  recovered_sessions,
});
const spawnCall = () =>
  expect(fetch).toHaveBeenCalledWith("/api/sessions/s1/acp/spawn", expect.objectContaining({ method: "POST" }));

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
  it("renders each incompatibility variant, with an install command only for installable ones", () => {
    const install_command = "npm install -g @agentclientprotocol/claude-agent-acp@latest";
    const cases: [React.ComponentProps<typeof StartupErrorScreen>["detail"], string[], boolean][] = [
      [incompatible(), ["0.32.0", "0.39.0", "@agentclientprotocol/claude-agent-acp"], true],
      [
        {
          kind: "missing_agent_info",
          expected_package: "@agentclientprotocol/claude-agent-acp",
          install_command,
          auto_install: true,
        },
        ["did not report its package version", "@agentclientprotocol/claude-agent-acp"],
        true,
      ],
      [
        {
          kind: "mismatched_agent_name",
          expected: "@agentclientprotocol/claude-agent-acp",
          received: "some-wrapper-script",
          install_command,
          auto_install: true,
        },
        ["@agentclientprotocol/claude-agent-acp", "some-wrapper-script"],
        true,
      ],
      [
        {
          kind: "unparseable_agent_version",
          package_name: "@agentclientprotocol/claude-agent-acp",
          raw_version: "not-semver",
          required: "0.39.0",
          install_command,
          auto_install: true,
        },
        ["not-semver", "0.39.0"],
        true,
      ],
      [{ kind: "unsupported_protocol_version", expected: "V1", received: "V2" }, ["ACP protocol", "V1", "V2"], false],
    ];
    for (const [detail, texts, hasCommand] of cases) {
      const { container, queryByTestId } = render(<StartupErrorScreen detail={detail} sessionId="s1" />);
      for (const t of texts) expect(container.textContent, detail.kind).toContain(t);
      const cmd = queryByTestId("startup-error-install-command");
      if (hasCommand) expect(cmd?.textContent, detail.kind).toContain(install_command);
      else expect(cmd, detail.kind).toBeNull();
      cleanup();
    }
  });

  it("Restart agent POSTs to /acp/spawn", async () => {
    const { getByTestId } = render(<StartupErrorScreen detail={incompatible()} sessionId="sess%2Fa" />);
    fireEvent.click(getByTestId("startup-error-restart"));
    expect(fetch).toHaveBeenCalledWith(
      "/api/sessions/sess%252Fa/acp/spawn",
      expect.objectContaining({ method: "POST" }),
    );
  });

  it("Restart agent surfaces a failed respawn", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 500, text: () => Promise.resolve("boom") }));
    const { getByTestId, container } = render(<StartupErrorScreen detail={incompatible()} sessionId="s1" />);
    fireEvent.click(getByTestId("startup-error-restart"));
    await waitFor(() => expect(container.textContent).toContain("Restart failed"));
  });

  it.each([
    [false, "acp.allow_agent_install"],
    [true, "inside the sandbox container"],
  ])(
    "shows a disabled Update & restart plus an enable hint when the install setting is off (sandboxed=%s)",
    async (isSandboxed, hintText) => {
      fetchSettings.mockResolvedValue({ acp: { allow_agent_install: false } });
      const { queryByTestId, findByTestId } = render(
        <StartupErrorScreen detail={incompatible(true)} sessionId="s1" isSandboxed={isSandboxed} />,
      );
      await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
      expect(queryByTestId("startup-error-update-restart")).toBeNull();
      const disabled = await findByTestId("startup-error-update-restart-disabled");
      expect((disabled as HTMLButtonElement).disabled).toBe(true);
      expect((await findByTestId("startup-error-enable-hint")).textContent).toContain(hintText);
    },
  );

  it("hides Update & restart entirely for non-npm agents even when the setting is on", async () => {
    fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
    const { queryByTestId } = render(<StartupErrorScreen detail={incompatible(false)} sessionId="s1" />);
    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    expect(queryByTestId("startup-error-update-restart")).toBeNull();
    expect(queryByTestId("startup-error-update-restart-disabled")).toBeNull();
    expect(queryByTestId("startup-error-enable-hint")).toBeNull();
  });

  it("Update & restart installs, respawns, and reports sessions queued for recovery", async () => {
    fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
    installAcpAgent.mockResolvedValue(installOk(3));
    const { findByTestId, container } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" />);
    fireEvent.click(await findByTestId("startup-error-update-restart"));
    expect(installAcpAgent).toHaveBeenCalledWith("s1");
    await waitFor(spawnCall);
    await waitFor(() => expect(container.textContent).toContain("3 other sessions"));
  });

  it.each([
    [
      "rejects",
      () => installAcpAgent.mockRejectedValue(new Error("npm is not on the daemon's PATH")),
      ["npm is not on the daemon's PATH"],
    ],
    [
      "exits non-zero",
      () =>
        installAcpAgent.mockResolvedValue({
          ...installOk(),
          success: false,
          exit_code: 243,
          stdout: "",
          stderr: "npm ERR! EACCES",
        }),
      ["Install exited with code 243", "npm ERR! EACCES"],
    ],
  ])("Update & restart surfaces the error and does not respawn when the install %s", async (_label, arrange, texts) => {
    fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
    arrange();
    const { findByTestId, container } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" />);
    fireEvent.click(await findByTestId("startup-error-update-restart"));
    await waitFor(() => expect(container.textContent).toContain(texts[0]));
    for (const t of texts) expect(container.textContent).toContain(t);
    expect(fetch).not.toHaveBeenCalled();
  });

  describe("sandboxed session (#2913)", () => {
    it("hides the host install command and shows the runtime-aware container note", () => {
      const { queryByTestId, getByTestId } = render(
        <StartupErrorScreen detail={incompatible()} sessionId="s1" isSandboxed />,
      );
      // The host copy-paste block is misleading in a sandbox, so it is gone.
      expect(queryByTestId("startup-error-install-command")).toBeNull();
      const note = getByTestId("startup-error-sandbox-note").textContent ?? "";
      expect(note).toContain("inside the container");
      expect(note).toContain("sandbox image update available");
      // A literal `docker pull` would be wrong for Podman / Apple Container or a custom image.
      expect(note).not.toContain("docker pull");
    });

    it("relabels the recovery button, then installs through the same endpoint and respawns", async () => {
      fetchSettings.mockResolvedValue({ acp: { allow_agent_install: true } });
      installAcpAgent.mockResolvedValue(installOk());
      const { findByTestId } = render(<StartupErrorScreen detail={incompatible(true)} sessionId="s1" isSandboxed />);
      const btn = await findByTestId("startup-error-update-restart");
      expect(btn.textContent).toContain("Update in sandbox & restart");
      fireEvent.click(btn);
      expect(installAcpAgent).toHaveBeenCalledWith("s1");
      await waitFor(spawnCall);
    });
  });
});

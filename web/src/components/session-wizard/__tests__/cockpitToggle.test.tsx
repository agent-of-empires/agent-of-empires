// @vitest-environment jsdom
//
// Covers the per-session cockpit opt-out added in #1580: when the
// cockpit master switch is on, the wizard now lets the user create a
// plain tmux/terminal session for an ACP-capable tool instead of being
// force-routed into cockpit. Two surfaces:
//
//   - AgentStep renders an interactive CockpitSubstrateCard (a switch,
//     default on) for ACP-capable tools (built-in or custom); non-ACP
//     tools keep the read-only fallback notice and show no switch.
//   - SessionWizard's submit payload sets `cockpit_mode` from
//     `cockpitMasterEnabled && acpCapable && useCockpit`, so toggling
//     the switch off sends `cockpit_mode: false` (the server then
//     creates a tmux session even with the master switch on).
//
// The payload assertions are the request-permutation coverage the
// AGENTS.md mandate calls for; the live persistence path stays in the
// Playwright suite.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, fireEvent, waitFor } from "@testing-library/react";

import { AgentStep } from "../steps/AgentStep";
import { SessionWizard } from "../SessionWizard";
import { initialData } from "../wizardReducer";
import type { AgentInfo, ProfileInfo } from "../../../lib/types";

const createSession = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchSettings: vi.fn().mockResolvedValue({}),
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: vi.fn().mockResolvedValue({ available: false }),
  fetchProfiles: vi.fn().mockResolvedValue([]),
  createSession: (...args: unknown[]) => createSession(...args),
}));

afterEach(() => {
  cleanup();
});

const claude: AgentInfo = {
  kind: "builtin",
  name: "claude",
  binary: "claude",
  host_only: false,
  installed: true,
  install_hint: "",
};

const nonAcpBuiltin: AgentInfo = {
  kind: "builtin",
  name: "aider",
  binary: "aider",
  host_only: false,
  installed: true,
  install_hint: "",
};

const custom: AgentInfo = {
  kind: "custom",
  name: "remote-helper",
  binary: "remote-helper",
  host_only: false,
  installed: true,
  install_hint: "Configured custom agent",
};

function renderAgentStep(overrides: {
  tool?: string;
  agents?: AgentInfo[];
  cockpitMasterEnabled?: boolean;
  useCockpit?: boolean;
}) {
  const onChange = vi.fn();
  const utils = render(
    <AgentStep
      data={{
        ...initialData,
        tool: overrides.tool ?? "claude",
        useCockpit: overrides.useCockpit ?? true,
      }}
      onChange={onChange}
      agents={overrides.agents ?? [claude, nonAcpBuiltin, custom]}
      profiles={[] as ProfileInfo[]}
      dockerAvailable={false}
      onApplyProfileDefaults={() => {}}
      cockpitMasterEnabled={overrides.cockpitMasterEnabled ?? true}
    />,
  );
  return { onChange, ...utils };
}

describe("AgentStep cockpit substrate card (#1580)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders an interactive switch (default on) for an ACP-capable built-in when the master switch is on", () => {
    const { getByRole } = renderAgentStep({ tool: "claude" });
    const toggle = getByRole("switch", { name: "Use cockpit" });
    expect(toggle.getAttribute("aria-checked")).toBe("true");
  });

  it("toggling the switch off calls onChange('useCockpit', false)", () => {
    const { onChange, getByRole } = renderAgentStep({ tool: "claude" });
    fireEvent.click(getByRole("switch", { name: "Use cockpit" }));
    expect(onChange).toHaveBeenCalledWith("useCockpit", false);
  });

  it("reflects useCockpit=false as an unchecked switch", () => {
    const { getByRole } = renderAgentStep({ tool: "claude", useCockpit: false });
    expect(getByRole("switch", { name: "Use cockpit" }).getAttribute("aria-checked")).toBe("false");
  });

  it("shows no switch for a non-ACP built-in, only the terminal fallback notice", () => {
    const { queryByRole, getByText } = renderAgentStep({ tool: "aider" });
    expect(queryByRole("switch", { name: "Use cockpit" })).toBeNull();
    expect(getByText(/has no ACP adapter yet/)).toBeTruthy();
  });

  it("shows no switch for a custom agent, only the fallback notice", () => {
    const { queryByRole, getByText } = renderAgentStep({ tool: "remote-helper" });
    expect(queryByRole("switch", { name: "Use cockpit" })).toBeNull();
    expect(
      getByText(
        "Custom agents run in the terminal unless they define agent_cockpit_cmd in config or TUI settings.",
      ),
    ).toBeTruthy();
  });

  it("shows neither switch nor notice when the master switch is off", () => {
    const { queryByRole, queryByText } = renderAgentStep({
      tool: "claude",
      cockpitMasterEnabled: false,
    });
    expect(queryByRole("switch", { name: "Use cockpit" })).toBeNull();
    expect(queryByText(/Renders the agent's plan/)).toBeNull();
  });
});

describe("SessionWizard cockpit_mode payload (#1580)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    createSession.mockResolvedValue({ ok: true, session: { warnings: [] } });
  });

  function renderWizard(cockpitMasterEnabled: boolean) {
    return render(
      <SessionWizard
        onClose={() => {}}
        onCreated={() => {}}
        prefill={{ skipToReview: true, path: "/tmp/proj", tool: "claude" }}
        cockpitMasterEnabled={cockpitMasterEnabled}
      />,
    );
  }

  it("sends cockpit_mode: true for an ACP tool when master on and the toggle is left on (default)", async () => {
    const { getByText } = renderWizard(true);
    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(createSession).toHaveBeenCalledWith(
      expect.objectContaining({ tool: "claude", cockpit_mode: true }),
    );
  });

  it("sends cockpit_mode: false when master on but the user opts out via the toggle", async () => {
    const { getByText, getByRole } = renderWizard(true);
    // Jump from review back to the agent step via the Interface row,
    // flip the cockpit switch off, return to review, and launch.
    fireEvent.click(getByText("Interface"));
    fireEvent.click(getByRole("switch", { name: "Use cockpit" }));
    fireEvent.click(getByText("Next"));
    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(createSession).toHaveBeenCalledWith(
      expect.objectContaining({ tool: "claude", cockpit_mode: false }),
    );
  });

  it("sends cockpit_mode: false when the master switch is off", async () => {
    const { getByText } = renderWizard(false);
    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(createSession).toHaveBeenCalledWith(
      expect.objectContaining({ tool: "claude", cockpit_mode: false }),
    );
  });
});

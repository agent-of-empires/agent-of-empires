// @vitest-environment jsdom
//
// Covers the per-session structured-view opt-out: the wizard lets the user
// create a terminal-view session for an ACP-capable tool instead of the
// default structured view. Two surfaces:
//
//   - AgentStep renders an interactive ViewPickerCard (a switch,
//     default on) for ACP-capable tools (built-in or custom); non-ACP
//     tools keep the read-only fallback notice and show no switch.
//   - SessionWizard's submit payload sets `structured_view` from
//     `acpCapable && useStructuredView`, so toggling the switch off sends
//     the server then creates a terminal-view session.
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
import { fetchSettings } from "../../../lib/api";

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
  vi.useRealTimers();
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

const cursor: AgentInfo = {
  kind: "builtin",
  name: "cursor",
  binary: "cursor-agent",
  host_only: false,
  installed: true,
  install_hint: "",
  acp_capable: true,
  acp_command: "cursor-agent",
  acp_args: ["acp"],
};

function renderAgentStep(overrides: {
  tool?: string;
  agents?: AgentInfo[];
  useStructuredView?: boolean;
  agentModel?: string;
}) {
  const onChange = vi.fn();
  const utils = render(
    <AgentStep
      data={{
        ...initialData,
        tool: overrides.tool ?? "claude",
        useStructuredView: overrides.useStructuredView ?? true,
        agentModel: overrides.agentModel ?? "",
      }}
      onChange={onChange}
      agents={overrides.agents ?? [claude, nonAcpBuiltin, custom]}
      profiles={[] as ProfileInfo[]}
      dockerAvailable={false}
      onApplyProfileDefaults={() => {}}
    />,
  );
  return { onChange, ...utils };
}

describe("AgentStep structured-view view card", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders an interactive switch (default on) for an ACP-capable built-in", () => {
    const { getByRole } = renderAgentStep({ tool: "claude" });
    const toggle = getByRole("switch", { name: "Use structured view" });
    expect(toggle.getAttribute("aria-checked")).toBe("true");
  });

  it("toggling the switch off calls onChange('useStructuredView', false)", () => {
    const { onChange, getByRole } = renderAgentStep({ tool: "claude" });
    fireEvent.click(getByRole("switch", { name: "Use structured view" }));
    expect(onChange).toHaveBeenCalledWith("useStructuredView", false);
  });

  it("reflects useStructuredView=false as an unchecked switch", () => {
    const { getByRole } = renderAgentStep({
      tool: "claude",
      useStructuredView: false,
    });
    expect(
      getByRole("switch", { name: "Use structured view" }).getAttribute(
        "aria-checked",
      ),
    ).toBe("false");
  });

  it("renders Cursor model controls in the session wizard", () => {
    const { getByRole } = renderAgentStep({
      tool: "cursor",
      agents: [cursor],
    });

    expect(getByRole("combobox", { name: "Model" })).toBeTruthy();
    expect(getByRole("switch", { name: "Fast mode" })).toBeTruthy();
  });

  it("clears a pending Cursor model blur timer on unmount", () => {
    vi.useFakeTimers();
    const clearTimeoutSpy = vi.spyOn(window, "clearTimeout");
    const { getByRole, unmount } = renderAgentStep({
      tool: "cursor",
      agents: [cursor],
    });
    const modelInput = getByRole("combobox", { name: "Model" });

    fireEvent.focus(modelInput);
    fireEvent.blur(modelInput);
    unmount();

    expect(clearTimeoutSpy).toHaveBeenCalled();
    clearTimeoutSpy.mockRestore();
  });

  it("shows no switch for a non-ACP built-in, only the terminal fallback notice", () => {
    const { queryByRole, getByText } = renderAgentStep({ tool: "aider" });
    expect(queryByRole("switch", { name: "Use structured view" })).toBeNull();
    expect(getByText(/has no ACP adapter yet/)).toBeTruthy();
  });

  it("shows no switch for a custom agent, only the fallback notice", () => {
    const { queryByRole, getByText } = renderAgentStep({
      tool: "remote-helper",
    });
    expect(queryByRole("switch", { name: "Use structured view" })).toBeNull();
    expect(
      getByText(
        "Custom agents run in the terminal unless they define agent_acp_cmd in config or TUI settings.",
      ),
    ).toBeTruthy();
  });
});

describe("SessionWizard structured_view payload", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    createSession.mockResolvedValue({ ok: true, session: { warnings: [] } });
  });

  function renderWizard(tool = "claude") {
    return render(
      <SessionWizard
        onClose={() => {}}
        onCreated={() => {}}
        prefill={{ skipToReview: true, path: "/tmp/proj", tool }}
      />,
    );
  }

  function renderWizardWithoutToolPrefill() {
    return render(
      <SessionWizard
        onClose={() => {}}
        onCreated={() => {}}
        prefill={{ skipToReview: true, path: "/tmp/proj" }}
      />,
    );
  }

  it("sends the structured view for an ACP tool when the toggle is left on (default)", async () => {
    const { getByText } = renderWizard();
    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(createSession).toHaveBeenCalledWith(
      expect.objectContaining({ tool: "claude", view: "structured" }),
    );
  });

  it("sends the terminal view when the user opts out via the toggle", async () => {
    const { getByText, getByRole } = renderWizard();
    // Jump from review back to the agent step via the Interface row,
    // flip the structured-view switch off, return to review, and launch.
    fireEvent.click(getByText("Interface"));
    fireEvent.click(getByRole("switch", { name: "Use structured view" }));
    fireEvent.click(getByText("Next"));
    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(createSession).toHaveBeenCalledWith(
      expect.objectContaining({ tool: "claude", view: "terminal" }),
    );
  });

  it("sends profile-resolved agent model and effort defaults", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({
      session: {
        default_tool: "opencode",
        acp_defaults: {
          opencode: { model: "openai/gpt-5.5", effort: "high" },
        },
      },
      sandbox: {},
    } as never);
    const { getAllByText, getByText } = renderWizardWithoutToolPrefill();
    // "opencode" now renders in both the Agent row and the resolved
    // Launch command row (#1911), so match either occurrence.
    await waitFor(() =>
      expect(getAllByText(/opencode/).length).toBeGreaterThan(0),
    );
    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(createSession).toHaveBeenCalledWith(
      expect.objectContaining({
        tool: "opencode",
        view: "structured",
        agent_model: "openai/gpt-5.5",
        agent_effort: "high",
      }),
    );
  });

  it("sends a custom default structured-view agent_name from profile settings", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({
      acp: {
        default_agent: "gjc",
      },
      session: {
        default_tool: "opencode",
        agent_acp_cmd: {
          gjc: "gjc --mode acp",
        },
      },
      sandbox: {},
    } as never);
    const { getAllByText, getByText } = renderWizardWithoutToolPrefill();
    await waitFor(() =>
      expect(getAllByText(/opencode/).length).toBeGreaterThan(0),
    );

    fireEvent.click(getByText(/Launch session/));

    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(createSession).toHaveBeenCalledWith(
      expect.objectContaining({
        tool: "opencode",
        view: "structured",
        agent_name: "gjc",
      }),
    );
  });

  it("does not send Cursor model defaults from the session wizard", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({
      session: {
        default_tool: "cursor",
        acp_defaults: {
          cursor: { model: "composer-2.5-fast", effort: "high" },
        },
      },
      sandbox: {},
    } as never);
    const { getAllByText, getByText } = renderWizardWithoutToolPrefill();
    await waitFor(() =>
      expect(getAllByText(/cursor/).length).toBeGreaterThan(0),
    );
    fireEvent.click(getByText(/Launch session/));
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(createSession).toHaveBeenCalledWith(
      expect.objectContaining({
        tool: "cursor",
        view: "structured",
      }),
    );
    expect(createSession.mock.calls[0][0].agent_model).toBeUndefined();
    expect(createSession.mock.calls[0][0].agent_effort).toBeUndefined();
  });
});

// @vitest-environment jsdom
//
// Covers the AgentStep + ReviewStep changes that opened up the wizard
// to custom-agent selections (#1252):
//
//   - AgentStep selects both kind="custom" entries and `installed`
//     built-ins for the picker grid.
//   - AgentStep renders a "Custom" badge for kind="custom".
//   - AgentStep's ViewNotice branches to a custom-agent string
//     when the selected agent's kind is "custom".
//   - ReviewStep's AgentReviewValue renders just the name for built-in
//     agents but adds a "Custom" badge for kind="custom".
//
// Vitest is sufficient here because the changed surface is pure
// rendering; the live persistence path is covered separately by
// web/tests/wizard-custom-agent.spec.ts.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, fireEvent } from "@testing-library/react";

import { AgentStep } from "../steps/AgentStep";
import { ReviewStep } from "../steps/ReviewStep";
import { initialData } from "../wizardReducer";
import type { AgentInfo, ProfileInfo } from "../../../lib/types";

vi.mock("../../../lib/api", () => ({
  fetchSettings: vi.fn().mockResolvedValue({}),
}));

afterEach(() => {
  cleanup();
});

const builtin: AgentInfo = {
  kind: "builtin",
  name: "claude",
  binary: "claude",
  host_only: false,
  installed: true,
  install_hint: "",
  acp_capable: true,
};

const custom: AgentInfo = {
  kind: "custom",
  name: "remote-helper",
  binary: "remote-helper",
  host_only: false,
  installed: true,
  install_hint: "Configured custom agent",
  acp_capable: false,
};

// A custom agent with an agent_acp_cmd configured: the server marks
// it acp_capable, so the wizard offers structured view instead of the terminal.
const acpCustom: AgentInfo = {
  kind: "custom",
  name: "oc-superpowers",
  binary: "oc-superpowers",
  host_only: false,
  installed: true,
  install_hint: "Configured custom agent",
  acp_capable: true,
};

const uninstalledBuiltin: AgentInfo = {
  kind: "builtin",
  name: "uninstalled-builtin",
  binary: "uninstalled-builtin",
  host_only: false,
  installed: false,
  install_hint: "brew install x",
  acp_capable: false,
};

function renderAgentStep(overrides: { tool?: string; agents?: AgentInfo[] }) {
  const onChange = vi.fn();
  const utils = render(
    <AgentStep
      data={{ ...initialData, tool: overrides.tool ?? "claude" }}
      onChange={onChange}
      agents={overrides.agents ?? [builtin, custom]}
      profiles={[] as ProfileInfo[]}
      dockerAvailable={false}
      onApplyProfileDefaults={() => {}}
    />,
  );
  return { onChange, ...utils };
}

describe("AgentStep custom-agent selection (#1252)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("shows custom agents in the picker with a Custom badge and hides uninstalled built-ins", () => {
    const { getByRole, queryByRole, queryAllByText } = renderAgentStep({
      tool: "claude",
      agents: [builtin, custom, uninstalledBuiltin],
    });

    expect(getByRole("button", { name: /claude/ })).toBeTruthy();
    expect(getByRole("button", { name: /remote-helper/ })).toBeTruthy();
    expect(
      queryByRole("button", { name: "uninstalled-builtin", exact: true }),
    ).toBeNull();
    expect(queryAllByText("Custom").length).toBeGreaterThan(0);
  });

  it("hides the No agents installed warning when only a custom agent is configured", () => {
    const { queryByText } = renderAgentStep({
      tool: "remote-helper",
      agents: [custom],
    });
    expect(queryByText("No agents installed")).toBeNull();
  });

  it("renders the terminal-fallback notice for a custom agent with no agent_acp_cmd", () => {
    const { getByText } = renderAgentStep({
      tool: "remote-helper",
      agents: [builtin, custom],
    });
    expect(
      getByText(
        /Custom agents run in the terminal unless they define agent_acp_cmd/,
      ),
    ).toBeTruthy();
  });

  it("renders the structured view card for a custom agent that is acp_capable", () => {
    // A custom agent with agent_acp_cmd (acp_capable=true) must offer
    // structured view, not the terminal fallback.
    const { getByRole, getByText, queryByText } = renderAgentStep({
      tool: "oc-superpowers",
      agents: [builtin, acpCustom],
    });
    expect(getByRole("switch", { name: "Use structured view" })).toBeTruthy();
    expect(
      getByText(/Renders the agent's plan, tool calls, and diffs/),
    ).toBeTruthy();
    expect(queryByText(/Custom agents run in the terminal/)).toBeNull();
  });

  it("renders the interactive structured view toggle when the selected agent is a built-in with ACP support", () => {
    const { getByRole, getByText } = renderAgentStep({
      tool: "claude",
      agents: [builtin, custom],
    });
    // The ACP-capable case now renders ViewPickerCard (an
    // interactive switch defaulting on) rather than a read-only notice.
    expect(getByRole("switch", { name: "Use structured view" })).toBeTruthy();
    expect(getByText(/Renders the agent's plan/)).toBeTruthy();
  });

  it("clicking an agent button calls onChange with the agent name", () => {
    const { onChange, getByRole } = renderAgentStep({
      tool: "claude",
      agents: [builtin, custom],
    });
    fireEvent.click(getByRole("button", { name: /remote-helper/ }));
    expect(onChange).toHaveBeenCalledWith("tool", "remote-helper");
  });
});

describe("AgentStep profile description (#949)", () => {
  function renderWithProfiles(
    profiles: ProfileInfo[],
    dataOverrides: Partial<typeof initialData> = {},
  ) {
    const onChange = vi.fn();
    const onApplyProfileDefaults = vi.fn();
    const utils = render(
      <AgentStep
        data={{ ...initialData, tool: "claude", ...dataOverrides }}
        onChange={onChange}
        agents={[builtin]}
        profiles={profiles}
        dockerAvailable={false}
        onApplyProfileDefaults={onApplyProfileDefaults}
      />,
    );
    return { onChange, onApplyProfileDefaults, ...utils };
  }

  it("renders each profile's description as helper text under its name", () => {
    const { getByText } = renderWithProfiles([
      {
        name: "default",
        is_default: true,
        description: "Stock setup, no overrides",
      },
      {
        name: "yolo-sandbox",
        is_default: false,
        description: "Auto-approve in a container",
      },
    ]);
    expect(getByText("Stock setup, no overrides")).toBeTruthy();
    expect(getByText("Auto-approve in a container")).toBeTruthy();
  });

  it("omits the helper text line when a profile has no description", () => {
    const { queryByText, getByRole } = renderWithProfiles([
      { name: "default", is_default: true },
      { name: "other", is_default: false },
    ]);
    // The card itself is still rendered ...
    expect(getByRole("radio", { name: /other/ })).toBeTruthy();
    // ... but no description text leaks through with a stray "undefined".
    expect(queryByText(/undefined/)).toBeNull();
  });

  it("clicking a profile card calls onChange with the profile name", () => {
    const { onChange, getByRole } = renderWithProfiles([
      { name: "default", is_default: true },
      { name: "work", is_default: false, description: "Work setup" },
    ]);
    fireEvent.click(getByRole("radio", { name: /work/ }));
    expect(onChange).toHaveBeenCalledWith("profile", "work");
  });

  it("renders the Active badge on the profile flagged is_default", () => {
    // is_default true is rendered as an "Active" pill; checks that the
    // conditional badge branch is exercised in coverage.
    const { getAllByText } = renderWithProfiles([
      { name: "default", is_default: true, description: "Stock setup" },
      { name: "work", is_default: false, description: "Work setup" },
    ]);
    expect(getAllByText("Active").length).toBe(1);
  });

  it("marks the currently selected profile with aria-checked=true", () => {
    // data.profile === p.name takes the selected styling/aria branch.
    const { getByRole } = renderWithProfiles(
      [
        { name: "default", is_default: true },
        { name: "work", is_default: false, description: "Work setup" },
      ],
      { profile: "work" },
    );
    const selected = getByRole("radio", { name: /work/ });
    expect(selected.getAttribute("aria-checked")).toBe("true");
    const unselected = getByRole("radio", { name: /^Server default/ });
    expect(unselected.getAttribute("aria-checked")).toBe("false");
  });

  it("clicking Server default with a profile selected calls onChange with empty string", () => {
    // The "Server default" card uses handleProfileChange("") and bails
    // before fetchSettings, so it must not call onApplyProfileDefaults.
    const { onChange, onApplyProfileDefaults, getByRole } = renderWithProfiles(
      [
        { name: "default", is_default: true },
        { name: "work", is_default: false },
      ],
      { profile: "work" },
    );
    fireEvent.click(getByRole("radio", { name: /Server default/ }));
    expect(onChange).toHaveBeenCalledWith("profile", "");
    expect(onApplyProfileDefaults).not.toHaveBeenCalled();
  });

  it("shows the (Custom) marker when the selected profile has been edited", () => {
    const { getByText } = renderWithProfiles(
      [
        { name: "default", is_default: true },
        { name: "work", is_default: false },
      ],
      { profile: "work", profileDirty: true },
    );
    expect(
      getByText(/\(Custom\) Settings differ from preset defaults/),
    ).toBeTruthy();
  });

  it("confirms before switching profiles when settings are dirty (canceled)", () => {
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(false);
    try {
      const { onChange, getByRole } = renderWithProfiles(
        [
          { name: "default", is_default: true },
          { name: "work", is_default: false },
        ],
        { profile: "default", profileDirty: true },
      );
      fireEvent.click(getByRole("radio", { name: /work/ }));
      expect(confirmSpy).toHaveBeenCalled();
      // User cancelled, so no profile change should fire.
      expect(onChange).not.toHaveBeenCalledWith("profile", "work");
    } finally {
      confirmSpy.mockRestore();
    }
  });

  it("hides the profile picker when only a single profile exists", () => {
    // Guard the showProfilePicker branch: list of length <= 1 hides the
    // picker entirely so the Workflow preset section is not rendered.
    const { queryByText } = renderWithProfiles([
      { name: "default", is_default: true, description: "Stock setup" },
    ]);
    expect(queryByText("Workflow preset")).toBeNull();
  });
});

describe("ReviewStep agent row (#1252)", () => {
  function renderReviewStep(overrides: { tool: string; agents?: AgentInfo[] }) {
    return render(
      <ReviewStep
        data={{
          ...initialData,
          path: "/tmp/example",
          title: "session",
          tool: overrides.tool,
        }}
        onChange={() => {}}
        agents={overrides.agents ?? [builtin, custom]}
        isSubmitting={false}
        error={null}
        onSubmit={() => {}}
        onJumpTo={() => {}}
        steps={
          [{ id: "agent", label: "Agent" }] as Parameters<
            typeof ReviewStep
          >[0]["steps"]
        }
      />,
    );
  }

  it("renders just the agent name when the selected agent is built-in", () => {
    const { queryAllByText } = renderReviewStep({ tool: "claude" });
    // The "Custom" badge text should not appear in the review row.
    expect(queryAllByText("Custom").length).toBe(0);
  });

  it("renders a Custom badge in the agent row when the selected agent is custom", () => {
    const { queryAllByText } = renderReviewStep({ tool: "remote-helper" });
    expect(queryAllByText("Custom").length).toBeGreaterThan(0);
  });

  it("falls back to '(not set)' without crashing when data.tool does not match any agent", () => {
    const { getByText } = renderReviewStep({ tool: "" });
    expect(getByText("(not set)")).toBeTruthy();
  });
});

// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { AgentOptions } from "../steps/AgentOptions";
import { AgentPickerEssentials } from "../steps/AgentPickerEssentials";
import { initialData, type WizardData } from "../wizardReducer";
import type { AgentInfo, ProfileInfo } from "../../../lib/types";
import { agent } from "./fixtures";

vi.mock("../../../lib/api", () => ({
  fetchSettings: vi.fn().mockResolvedValue({}),
}));

afterEach(cleanup);

const claude = agent("claude");
const custom = agent("remote-helper", { kind: "custom", acp_capable: false, install_hint: "Configured custom agent" });
const acpCustom = agent("oc-superpowers", { kind: "custom" });
const aider = agent("aider", { acp_capable: false });

function renderOptions(
  data: Partial<WizardData> = {},
  props: { agents?: AgentInfo[]; profiles?: ProfileInfo[]; dockerAvailable?: boolean } = {},
) {
  const onChange = vi.fn();
  const onApplyProfileDefaults = vi.fn();
  render(
    <AgentOptions
      data={{ ...initialData, ...data }}
      onChange={onChange}
      agents={props.agents ?? [claude, custom, acpCustom, aider]}
      profiles={props.profiles ?? []}
      dockerAvailable={props.dockerAvailable ?? false}
      onApplyProfileDefaults={onApplyProfileDefaults}
    />,
  );
  return { onChange, onApplyProfileDefaults };
}

const viewSwitch = () => screen.queryByRole("switch", { name: "Use structured view" });

describe("AgentPickerEssentials", () => {
  const renderPicker = (tool: string, agents: AgentInfo[]) => {
    const onChange = vi.fn();
    render(<AgentPickerEssentials data={{ ...initialData, tool }} onChange={onChange} agents={agents} />);
    return { onChange };
  };

  it("lists installed built-ins and custom agents with a Custom badge, and selects on click", () => {
    const { onChange } = renderPicker("claude", [claude, custom, agent("uninstalled", { installed: false })]);
    expect(screen.queryByRole("button", { name: "uninstalled", exact: true })).toBeNull();
    expect(screen.getAllByText("Custom").length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole("button", { name: /remote-helper/ }));
    expect(onChange).toHaveBeenCalledWith("tool", "remote-helper");
  });

  it("does not warn about missing agents when only a custom agent exists", () => {
    renderPicker("remote-helper", [custom]);
    expect(screen.queryByText("No agents installed")).toBeNull();
  });

  it("badges deprecated agents and warns only when one is selected", () => {
    const agents = [agent("gemini"), claude, agent("custom-tool")];
    renderPicker("claude", agents);
    expect(screen.getByTestId("wizard-agent-deprecated-badge-gemini")).toBeTruthy();
    expect(screen.queryByTestId("wizard-agent-deprecated-badge-claude")).toBeNull();
    expect(screen.queryByTestId("wizard-agent-deprecated-badge-custom-tool")).toBeNull();
    expect(screen.queryByTestId("wizard-agent-deprecated-warning")).toBeNull();
    cleanup();
    renderPicker("gemini", agents);
    expect(screen.getByTestId("wizard-agent-deprecated-warning").textContent).toContain(
      "consider switching to antigravity",
    );
  });

  it("prefers the server lifecycle over the static mirror", () => {
    renderPicker("self-hosted", [
      agent("self-hosted", {
        lifecycle: { state: "deprecated", since: "2026-01-01", note: "upstream shut down", replacement: null },
      }),
    ]);
    expect(screen.getByTestId("wizard-agent-deprecated-badge-self-hosted")).toBeTruthy();
    const warning = screen.getByTestId("wizard-agent-deprecated-warning");
    expect(warning.textContent).toContain("upstream shut down");
    expect(warning.textContent).not.toContain("consider switching to");
  });
});

describe("AgentOptions view card", () => {
  it.each(["claude", "oc-superpowers"])("offers a structured view switch for ACP-capable %s", (tool) => {
    renderOptions({ tool });
    expect(viewSwitch()?.getAttribute("aria-checked")).toBe("true");
    expect(screen.getByText(/Renders the agent's plan, tool calls, and diffs/)).toBeTruthy();
  });

  it("toggles off via the switch or the card row, and reflects an unchecked state", () => {
    const { onChange } = renderOptions();
    fireEvent.click(viewSwitch()!);
    expect(onChange).toHaveBeenLastCalledWith("useStructuredView", false);
    onChange.mockClear();
    fireEvent.click(screen.getByText("Structured view"));
    expect(onChange).toHaveBeenCalledWith("useStructuredView", false);
    cleanup();
    renderOptions({ useStructuredView: false });
    expect(viewSwitch()?.getAttribute("aria-checked")).toBe("false");
  });

  it("shows only the terminal fallback, with its reason, for agents without structured view", () => {
    for (const [tool, agents, text] of [
      ["aider", undefined, /has no ACP adapter yet/],
      ["remote-helper", undefined, /Custom agents run in the terminal unless they define agent_acp_cmd/],
      ["claude", [{ ...claude, acp_allowed: false }], /not on the operator's allowed agents list/],
    ] as const) {
      renderOptions({ tool }, { agents: agents && [...agents] });
      expect(viewSwitch()).toBeNull();
      expect(screen.getByText(text)).toBeTruthy();
      cleanup();
    }
  });
});

describe("AgentOptions workflow presets", () => {
  const PROFILES: ProfileInfo[] = [
    { name: "default", is_default: true, description: "Stock setup, no overrides" },
    { name: "work", is_default: false },
  ];

  it("selects a preset, and Server default clears it without applying defaults", () => {
    const { onChange, onApplyProfileDefaults } = renderOptions({ profile: "work" }, { profiles: PROFILES });
    fireEvent.click(screen.getByRole("radio", { name: /Server default/ }));
    expect(onChange).toHaveBeenCalledWith("profile", "");
    expect(onApplyProfileDefaults).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("radio", { name: /work/ }));
    expect(onChange).toHaveBeenCalledWith("profile", "work");
  });

  it("confirms before switching away from preset or view edits", () => {
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(false);
    try {
      for (const dirty of [{ profileDirty: true }, { structuredViewDirty: true }]) {
        confirmSpy.mockClear();
        const { onChange } = renderOptions({ profile: "default", ...dirty }, { profiles: PROFILES });
        fireEvent.click(screen.getByRole("radio", { name: /work/ }));
        expect(confirmSpy).toHaveBeenCalled();
        expect(onChange).not.toHaveBeenCalledWith("profile", "work");
        cleanup();
      }
    } finally {
      confirmSpy.mockRestore();
    }
  });

  it("hides the picker with a single profile", () => {
    renderOptions({}, { profiles: [PROFILES[0]!] });
    expect(screen.queryByText("Workflow preset")).toBeNull();
  });
});

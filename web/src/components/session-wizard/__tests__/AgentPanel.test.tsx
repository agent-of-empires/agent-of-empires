// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { AgentPanel } from "../steps/AgentPanel";
import { AgentPickerEssentials } from "../steps/AgentPickerEssentials";
import { initialData, type WizardData } from "../wizardReducer";
import type { AgentInfo } from "../../../lib/types";
import { agent } from "./fixtures";

afterEach(cleanup);

const claude = agent("claude");
const custom = agent("remote-helper", { kind: "custom", acp_capable: false, install_hint: "Configured custom agent" });
const acpCustom = agent("oc-superpowers", { kind: "custom" });
const aider = agent("aider", { acp_capable: false });

function renderPanel(data: Partial<WizardData> = {}) {
  const onChange = vi.fn();
  render(
    <AgentPanel data={{ ...initialData, ...data }} onChange={onChange} agents={[claude, custom, acpCustom, aider]} />,
  );
  return { onChange };
}

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

  it.each([
    ["gemini", true],
    ["claude", false],
  ])("badges and warns for deprecated agents (%s)", (tool, deprecated) => {
    renderPicker(tool, [agent("gemini"), claude, agent("custom-tool")]);
    expect(!!screen.queryByTestId(`wizard-agent-deprecated-badge-gemini`)).toBe(true);
    expect(screen.queryByTestId("wizard-agent-deprecated-badge-claude")).toBeNull();
    expect(screen.queryByTestId("wizard-agent-deprecated-badge-custom-tool")).toBeNull();
    const warning = screen.queryByTestId("wizard-agent-deprecated-warning");
    expect(!!warning).toBe(deprecated);
    if (warning) {
      expect(warning.textContent).toContain("since 2026-06-18");
      expect(warning.textContent).toContain("enterprise/API-key remain valid");
      expect(warning.textContent).toContain("consider switching to antigravity");
    }
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

describe("AgentPanel launch fields", () => {
  it.each([
    ["claude", true, true],
    ["claude", false, false],
    ["aider", true, false],
  ])("flags extra args as ignored only for a structured session (%s, view=%s)", (tool, useStructuredView, ignored) => {
    renderPanel({ tool, useStructuredView, extraArgs: "--verbose" });
    expect(!!screen.queryByTestId("extra-args-ignored")).toBe(ignored);
    expect(screen.getByTestId("resolved-launch-command").textContent).toContain(tool);
  });

  it("edits the instructions, args and override", () => {
    const { onChange } = renderPanel();
    fireEvent.change(screen.getByLabelText("Agent instructions"), { target: { value: "be terse" } });
    // Found by their visible labels, which proves the label is tied to the input.
    fireEvent.change(screen.getByLabelText("Additional arguments"), { target: { value: "--x" } });
    fireEvent.change(screen.getByLabelText("Command override"), { target: { value: "cc" } });
    expect(onChange.mock.calls).toEqual([
      ["customInstruction", "be terse"],
      ["extraArgs", "--x"],
      ["commandOverride", "cc"],
    ]);
  });
});

// @vitest-environment jsdom
//
// Covers the agent-lifecycle surface in the wizard's agent picker:
//
//   - Deprecated agents (gemini) render a "Deprecated" badge on their
//     card; Active agents and unknown keys do not.
//   - Selecting a deprecated agent shows the non-blocking warning line
//     with the since date, note, and replacement.
//   - A server-provided `lifecycle` on AgentInfo wins over the static
//     profile mirror (an unlisted agent marked deprecated server-side
//     still gets its badge).
//
// Vitest is sufficient: the changed surface is pure rendering.
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { AgentPickerEssentials } from "../steps/AgentPickerEssentials";
import { initialData } from "../wizardReducer";
import type { AgentInfo } from "../../../lib/types";

afterEach(() => {
  cleanup();
});

function agent(name: string, lifecycle?: AgentInfo["lifecycle"]): AgentInfo {
  return {
    kind: "builtin",
    name,
    binary: name,
    host_only: false,
    installed: true,
    install_hint: "",
    acp_capable: true,
    ...(lifecycle ? { lifecycle } : {}),
  };
}

const FIXTURE: AgentInfo[] = [agent("gemini"), agent("claude"), agent("custom-tool")];

function renderPicker(tool: string, agents: AgentInfo[] = FIXTURE) {
  render(<AgentPickerEssentials data={{ ...initialData, tool }} onChange={vi.fn()} agents={agents} />);
}

describe("AgentPickerEssentials lifecycle", () => {
  it("badges only deprecated agents", () => {
    // gemini is deprecated in the static mirror; claude is Active;
    // "custom-tool" has no mirror entry (unknown key resolves active).
    const cases = [
      ["gemini", true],
      ["claude", false],
      ["custom-tool", false],
    ] as const;
    for (const [name, deprecated] of cases) {
      renderPicker("claude");
      expect(!!screen.queryByTestId(`wizard-agent-deprecated-badge-${name}`)).toBe(deprecated);
      cleanup();
    }
  });

  it("warns when the selected agent is deprecated and stays silent otherwise", () => {
    const cases = [
      ["gemini", true],
      ["claude", false],
    ] as const;
    for (const [tool, expectWarning] of cases) {
      renderPicker(tool);
      const warning = screen.queryByTestId("wizard-agent-deprecated-warning");
      expect(!!warning).toBe(expectWarning);
      if (warning) {
        expect(warning.textContent).toContain("since 2026-06-18");
        expect(warning.textContent).toContain("enterprise/API-key remain valid");
        expect(warning.textContent).toContain("consider switching to antigravity");
      }
      cleanup();
    }
  });

  it("prefers the server lifecycle over the static mirror", () => {
    // An agent the static mirror does not list can still be marked
    // deprecated by the daemon (/api/agents), and the badge follows.
    renderPicker("self-hosted", [
      agent("self-hosted", {
        state: "deprecated",
        since: "2026-01-01",
        note: "upstream shut down",
        replacement: null,
      }),
    ]);
    expect(screen.getByTestId("wizard-agent-deprecated-badge-self-hosted")).not.toBeNull();
    const warning = screen.getByTestId("wizard-agent-deprecated-warning");
    expect(warning.textContent).toContain("upstream shut down");
    expect(warning.textContent).not.toContain("consider switching to");
  });
});

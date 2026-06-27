// @vitest-environment jsdom
import { fireEvent, render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { BackgroundAgent } from "../../../lib/acpTypes";

// The panel reads the live list from the useAcpSession store; mock it so
// the test drives the rendering purely from a fixed agent list.
const agentsMock = vi.fn<() => BackgroundAgent[]>(() => []);
vi.mock("../../../hooks/useAcpSession", () => ({
  useBackgroundAgents: () => agentsMock(),
}));

import { BackgroundAgentsPanel } from "../BackgroundAgentsPanel";

function agent(over: Partial<BackgroundAgent> = {}): BackgroundAgent {
  return {
    agentId: "a1",
    toolCallId: "task-1",
    description: "Map backend lifecycle",
    prompt: "do the thing",
    model: "claude-opus-4-8",
    status: "running",
    startedAt: new Date(Date.now() - 5000).toISOString(),
    endedAt: null,
    toolCount: 3,
    lastTool: "Read",
    lastText: "scanning files",
    result: null,
    warning: null,
    ...over,
  };
}

describe("BackgroundAgentsPanel", () => {
  it("shows an empty state with no agents", () => {
    agentsMock.mockReturnValue([]);
    const { container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(container.textContent).toContain("No background sub-agents launched yet");
  });

  it("lists a running agent with description, tool count, and last activity", () => {
    agentsMock.mockReturnValue([agent()]);
    const { container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(container.textContent).toContain("Background agents · 1");
    expect(container.textContent).toContain("Map backend lifecycle");
    expect(container.textContent).toContain("running");
    expect(container.textContent).toContain("3 tools");
    expect(container.textContent).toContain("scanning files");
    // Internal id never surfaces.
    expect(container.textContent).not.toContain("a1");
  });

  it("expands to reveal prompt, model, and result; never leaks the agent id", () => {
    agentsMock.mockReturnValue([
      agent({
        status: "completed",
        endedAt: new Date().toISOString(),
        result: "found 12 files",
      }),
    ]);
    const { container, getByRole } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(container.textContent).toContain("done");
    fireEvent.click(getByRole("button"));
    expect(container.textContent).toContain("do the thing");
    expect(container.textContent).toContain("claude-opus-4-8");
    expect(container.textContent).toContain("found 12 files");
  });

  it("orders running agents before finished ones", () => {
    agentsMock.mockReturnValue([
      agent({ agentId: "done", toolCallId: "t-done", description: "Done one", status: "completed" }),
      agent({ agentId: "run", toolCallId: "t-run", description: "Running one", status: "running" }),
    ]);
    const { container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    const runIdx = container.textContent!.indexOf("Running one");
    const doneIdx = container.textContent!.indexOf("Done one");
    expect(runIdx).toBeGreaterThanOrEqual(0);
    expect(runIdx).toBeLessThan(doneIdx);
  });
});

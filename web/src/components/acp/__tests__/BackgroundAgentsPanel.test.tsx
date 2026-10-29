// @vitest-environment jsdom
import { fireEvent, render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { BackgroundAgent } from "../../../lib/acpTypes";
import { writeClipboard } from "../../../lib/clipboard";

vi.mock("../../../lib/clipboard", () => ({ writeClipboard: vi.fn().mockResolvedValue(true) }));

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
    tools: [],
    lastTool: "Read",
    lastText: "scanning files",
    result: null,
    warning: null,
    ...over,
  };
}

describe("BackgroundAgentsPanel", () => {
  it("groups a workflow's agents under the run and its phases", () => {
    const wf = (phase: string) => ({ run_id: "wf_1", name: "issue-drafts", phase });
    const ended = new Date().toISOString();
    agentsMock.mockReturnValue([
      agent({ agentId: "p1", description: "plain task", status: "completed", endedAt: ended }),
      agent({ agentId: "d1", description: "draft:one", workflow: wf("Draft"), status: "completed", endedAt: ended }),
      agent({ agentId: "d2", description: "draft:two", workflow: wf("Draft"), status: "completed", endedAt: ended }),
      agent({ agentId: "v1", description: "verify:one", workflow: wf("Verify") }),
    ]);
    const { container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    const text = container.textContent ?? "";
    expect(text).toContain("Background agents · 4");
    expect(text).toContain("issue-drafts");
    expect(text).toContain("2 of 3 done");
    expect(text).toContain("Draft · 2/2");
    expect(text).toContain("Verify · 0/1");
    // The active run sorts first; its phases keep start order; plain agents stay ungrouped.
    const order = ["issue-drafts", "draft:one", "draft:two", "verify:one", "plain task"].map((s) => text.indexOf(s));
    expect(order).toEqual([...order].sort((a, b) => a - b));
    // Collapsing the run hides its agents only.
    const runHeader = [...container.querySelectorAll("button")].find((b) => b.textContent?.includes("issue-drafts"));
    fireEvent.click(runHeader!);
    expect(container.textContent).not.toContain("draft:one");
    expect(container.textContent).toContain("plain task");
  });

  it("lists a running agent with description, tool count, and last activity", () => {
    agentsMock.mockReturnValue([agent()]);
    const { container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(container.textContent).toContain("Background agents · 1");
    // Persistent hint: async-only scope + where the sync ones live.
    expect(container.textContent).toContain("Only async");
    expect(container.textContent).toContain("Synchronous sub-agents run inline in the transcript");
    expect(container.textContent).toContain("Map backend lifecycle");
    expect(container.textContent).toContain("running");
    expect(container.textContent).toContain("3 tools");
    expect(container.textContent).toContain("scanning files");
    // Internal id never surfaces.
    expect(container.textContent).not.toContain("a1");
  });

  it("expands to reveal prompt, model, result, and tool calls; never leaks the agent id", () => {
    // A finished agent, so the first button is the row toggle (no Stop).
    agentsMock.mockReturnValue([
      agent({
        status: "completed",
        endedAt: new Date().toISOString(),
        result: "found 12 files",
        tools: [
          { name: "Bash", title: "ls -la", ok: true },
          { name: "Read", title: "src/main.rs", ok: false },
          { name: "Grep", title: "tmux", ok: undefined },
        ],
      }),
    ]);
    const { container, getAllByRole } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(container.textContent).toContain("done");
    fireEvent.click(getAllByRole("button")[0]!);
    for (const text of [
      "do the thing",
      "claude-opus-4-8",
      "found 12 files",
      "tools · 3",
      "ls -la",
      "src/main.rs",
      "Grep",
    ]) {
      expect(container.textContent).toContain(text);
    }
    expect(container.textContent).not.toContain("a1");
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

  it("shows a Stop button for active agents that POSTs the session cancel", async () => {
    const fetchMock = vi.fn(() => Promise.resolve({ ok: true } as Response));
    vi.stubGlobal("fetch", fetchMock);
    agentsMock.mockReturnValue([agent({ status: "running" })]);
    const { getByRole } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    const stop = getByRole("button", { name: /stop/i });
    fireEvent.click(stop);
    expect(fetchMock).toHaveBeenCalledWith("/api/sessions/s-1/acp/cancel", { method: "POST" });
    vi.unstubAllGlobals();
  });

  it("hides the Stop button for finished or stalled agents (cancel would be a no-op)", () => {
    for (const over of [{ status: "completed", endedAt: new Date().toISOString() }, { status: "stalled" }] as const) {
      agentsMock.mockReturnValue([agent(over)]);
      const { queryByRole, unmount } = render(<BackgroundAgentsPanel sessionId="s-1" />);
      expect(queryByRole("button", { name: /stop/i }), over.status).toBeNull();
      unmount();
    }
  });

  it("opens a details modal showing the full prompt, result, and tools", () => {
    agentsMock.mockReturnValue([
      agent({
        status: "completed",
        endedAt: new Date().toISOString(),
        prompt: "a very long prompt that the narrow panel would clamp",
        result: "the complete final result text",
        tools: [{ name: "Bash", title: "ls -la", ok: true }],
      }),
    ]);
    const { getByRole, getByText } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    fireEvent.click(getByRole("button", { name: /open full details/i }));
    const dialog = getByRole("dialog");
    expect(dialog.textContent).toContain("a very long prompt that the narrow panel would clamp");
    expect(dialog.textContent).toContain("the complete final result text");
    expect(dialog.textContent).toContain("ls -la");
    // Closes on the X button.
    fireEvent.click(getByRole("button", { name: /close/i }));
    expect(() => getByText("the complete final result text")).toThrow();
  });
});

describe("BackgroundAgentsPanel copy", () => {
  it("copies the full prompt and result of an expanded agent", () => {
    const prompt = "line one\nline two\nline three\nline four\nline five\nline six";
    agentsMock.mockReturnValue([
      agent({ status: "completed", endedAt: new Date().toISOString(), prompt, result: "done" }),
    ]);
    const { getByRole } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    fireEvent.click(getByRole("button", { name: /Map backend lifecycle/ }));
    fireEvent.click(getByRole("button", { name: "Copy prompt" }));
    expect(writeClipboard).toHaveBeenLastCalledWith(prompt);
    fireEvent.click(getByRole("button", { name: "Copy result" }));
    expect(writeClipboard).toHaveBeenLastCalledWith("done");
  });
});

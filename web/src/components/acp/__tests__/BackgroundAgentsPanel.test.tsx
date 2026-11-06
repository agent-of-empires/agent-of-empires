// @vitest-environment jsdom
import { fireEvent, render, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { BackgroundAgent, BackgroundShell, WorkflowRun } from "../../../lib/acpTypes";
import { writeClipboard } from "../../../lib/clipboard";

vi.mock("../../../lib/clipboard", () => ({ writeClipboard: vi.fn().mockResolvedValue(true) }));

// The panel reads the live list from the useAcpSession store; mock it so
// the test drives the rendering purely from a fixed agent list.
const agentsMock = vi.fn<() => BackgroundAgent[]>(() => []);
const tasksMock = vi.fn<() => string[]>(() => []);
const runsMock = vi.fn<() => Record<string, WorkflowRun>>(() => ({}));
const shellsMock = vi.fn<() => BackgroundShell[]>(() => []);
const taskOutputMock = vi.fn();
vi.mock("../../../lib/api", () => ({ fetchTaskOutput: (...args: unknown[]) => taskOutputMock(...args) }));
vi.mock("../../../hooks/useAcpSession", () => ({
  useBackgroundAgents: () => agentsMock(),
  useBackgroundShells: () => shellsMock(),
  useBackgroundTasks: () => tasksMock(),
  useWorkflowRuns: () => runsMock(),
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

beforeEach(() => {
  shellsMock.mockReturnValue([]);
});

function shell(over: Partial<BackgroundShell> = {}): BackgroundShell {
  return { taskId: "bx1", command: "npm run dev", startedAt: null, endedAt: null, finished: false, ...over };
}

describe("BackgroundAgentsPanel shells", () => {
  it("lists a running shell when there are no sub-agents, instead of the empty state", () => {
    agentsMock.mockReturnValue([]);
    shellsMock.mockReturnValue([
      shell(),
      shell({ taskId: "bx0", command: "make build", finished: true, endedAt: new Date().toISOString() }),
    ]);
    const { container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    const text = container.textContent ?? "";
    expect(text).not.toContain("No background work yet");
    expect(text).toContain("Running shells · 1");
    expect(text).toContain("Recent shells · 1");
    expect(text.indexOf("npm run dev")).toBeLessThan(text.indexOf("make build"));
  });

  it("shows the empty state with no agents and no shells", () => {
    agentsMock.mockReturnValue([]);
    const { container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(container.textContent).toContain("No background work yet");
  });

  it("opens the command and its output", async () => {
    agentsMock.mockReturnValue([]);
    shellsMock.mockReturnValue([shell({ finished: true })]);
    taskOutputMock.mockResolvedValue({ exists: true, output: "listening on 3000", truncated: false });
    const { container, getByTitle } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    fireEvent.click(getByTitle("Open command and output"));
    await waitFor(() => expect(document.body.textContent).toContain("listening on 3000"));
    expect(document.body.textContent).toContain("npm run dev");
    expect(taskOutputMock).toHaveBeenCalledWith("s-1", "bx1");
    expect(container).toBeTruthy();
  });

  it("says when the output is not available", async () => {
    agentsMock.mockReturnValue([]);
    shellsMock.mockReturnValue([shell({ finished: true })]);
    taskOutputMock.mockResolvedValue({ exists: false, output: "", truncated: false });
    const { getByTitle } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    fireEvent.click(getByTitle("Open command and output"));
    await waitFor(() => expect(document.body.textContent).toContain("Output not available."));
  });
});

describe("BackgroundAgentsPanel shell dialog", () => {
  const openShell = async (over: Partial<BackgroundShell> = {}, out = {}) => {
    agentsMock.mockReturnValue([]);
    shellsMock.mockReturnValue([shell(over)]);
    taskOutputMock.mockResolvedValue({ exists: true, output: "line one", truncated: false, ...out });
    const view = render(<BackgroundAgentsPanel sessionId="s-1" />);
    const launcher = view.getByTitle("Open command and output");
    launcher.focus();
    fireEvent.click(launcher);
    await waitFor(() => expect(document.body.textContent).toContain("line one"));
    return { ...view, launcher };
  };

  it("closes on Escape and on a backdrop click, returning focus to the launcher", async () => {
    const { launcher } = await openShell({ finished: true });
    const dialog = document.querySelector('[role="dialog"]') as HTMLElement;
    expect(document.activeElement).toBe(dialog.firstElementChild);
    fireEvent.keyDown(window, { key: "Escape" });
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(document.activeElement).toBe(launcher);

    fireEvent.click(launcher);
    fireEvent.click(document.querySelector('[role="dialog"]') as HTMLElement);
    expect(document.querySelector('[role="dialog"]')).toBeNull();
  });

  it("keeps Tab inside the dialog", async () => {
    await openShell({ finished: true });
    const buttons = [...(document.querySelector('[role="dialog"]') as HTMLElement).querySelectorAll("button")];
    const last = buttons[buttons.length - 1]!;
    last.focus();
    fireEvent.keyDown(window, { key: "Tab" });
    expect(document.activeElement).toBe(buttons[0]);
    fireEvent.keyDown(window, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(last);
  });

  it("toggles wrapping, flags a truncated tail and says when the command is unknown", async () => {
    await openShell({ command: "", finished: true }, { truncated: true });
    expect(document.body.textContent).toContain("(command unavailable)");
    expect(document.body.textContent).toContain("Showing the end of a long output.");
    const pre = document.querySelector("pre[class*='max-h']") as HTMLElement;
    expect(pre.className).toContain("whitespace-pre-wrap");
    fireEvent.click(document.querySelector("button[aria-pressed]") as HTMLElement);
    expect(pre.className).not.toContain("whitespace-pre-wrap");
  });

  it("polls a running shell one read at a time and stops once it finishes", async () => {
    vi.useFakeTimers();
    try {
      agentsMock.mockReturnValue([]);
      shellsMock.mockReturnValue([shell()]);
      let release: (v: unknown) => void = () => {};
      taskOutputMock.mockReset();
      taskOutputMock
        .mockImplementationOnce(() => new Promise((r) => (release = r)))
        .mockResolvedValue({ exists: true, output: "later", truncated: false });
      const view = render(<BackgroundAgentsPanel sessionId="s-1" />);
      fireEvent.click(view.getByTitle("Open command and output"));
      await vi.advanceTimersByTimeAsync(10_000);
      expect(taskOutputMock).toHaveBeenCalledTimes(1);

      release({ exists: true, output: "first", truncated: false });
      await vi.advanceTimersByTimeAsync(2000);
      expect(taskOutputMock).toHaveBeenCalledTimes(2);

      shellsMock.mockReturnValue([shell({ finished: true })]);
      view.rerender(<BackgroundAgentsPanel sessionId="s-1" />);
      await vi.advanceTimersByTimeAsync(0);
      const afterFinish = taskOutputMock.mock.calls.length;
      await vi.advanceTimersByTimeAsync(10_000);
      expect(taskOutputMock).toHaveBeenCalledTimes(afterFinish);
    } finally {
      vi.useRealTimers();
    }
  });
});

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

  it("keeps a run live between phases, until its task ends", () => {
    const ended = new Date().toISOString();
    agentsMock.mockReturnValue([
      agent({
        workflow: { run_id: "wf_1", name: "issue-drafts", phase: "Draft" },
        status: "completed",
        endedAt: ended,
      }),
    ]);
    runsMock.mockReturnValue({ "task-1": { taskId: "wfx", name: "issue-drafts", summary: null } });
    const runDot = (container: HTMLElement) =>
      [...container.querySelectorAll("button")]
        .find((b) => b.textContent?.includes("issue-drafts"))!
        .querySelector("span")!.className;
    tasksMock.mockReturnValue(["wfx"]);
    const gap = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(runDot(gap.container)).toContain("animate-pulse");
    gap.unmount();
    tasksMock.mockReturnValue([]);
    const finished = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(runDot(finished.container)).not.toContain("animate-pulse");
    runsMock.mockReturnValue({});
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

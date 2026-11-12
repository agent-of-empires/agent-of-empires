// @vitest-environment jsdom
import { act, fireEvent, render, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { BackgroundAgent, BackgroundShell, SubagentRecord, WorkflowRun } from "../../../lib/acpTypes";
import { writeClipboard } from "../../../lib/clipboard";

vi.mock("../../../lib/clipboard", () => ({ writeClipboard: vi.fn().mockResolvedValue(true) }));

// The panel reads the live list from the useAcpSession store; mock it so
// the test drives the rendering purely from a fixed agent list.
const agentsMock = vi.fn<() => BackgroundAgent[]>(() => []);
const tasksMock = vi.fn<() => string[]>(() => []);
const runsMock = vi.fn<() => Record<string, WorkflowRun>>(() => ({}));
const subagentsMock = vi.fn<() => SubagentRecord[]>(() => []);
const shellsMock = vi.fn<() => BackgroundShell[]>(() => []);
const taskOutputMock = vi.fn();
vi.mock("../../../lib/api", async (importActual) => ({
  ...(await importActual<typeof import("../../../lib/api")>()),
  fetchTaskOutput: (...args: unknown[]) => taskOutputMock(...args),
}));
vi.mock("../../../hooks/useAcpSession", () => ({
  useBackgroundAgents: () => agentsMock(),
  useBackgroundShells: () => shellsMock(),
  useBackgroundTasks: () => tasksMock(),
  useWorkflowRuns: () => runsMock(),
  useSubagents: () => subagentsMock(),
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
    const { getByTitle } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    fireEvent.click(getByTitle("Open command and output"));
    await waitFor(() => expect(document.body.textContent).toContain("listening on 3000"));
    expect(document.body.textContent).toContain("npm run dev");
    expect(taskOutputMock).toHaveBeenCalledWith("s-1", "bx1");
  });

  it("keeps an open shell dialog when the first background agent appears", async () => {
    agentsMock.mockReturnValue([]);
    shellsMock.mockReturnValue([shell({ finished: true })]);
    taskOutputMock.mockResolvedValue({ exists: true, output: "listening on 3000", truncated: false });
    const view = render(<BackgroundAgentsPanel sessionId="s-1" />);
    fireEvent.click(view.getByTitle("Open command and output"));
    await waitFor(() => expect(view.getByRole("dialog")).toBeTruthy());

    agentsMock.mockReturnValue([agent()]);
    view.rerender(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(view.getByText("Background agents · 1")).toBeTruthy();
    expect(view.getByRole("dialog")).toBeTruthy();
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
    const stops = [
      ...(document.querySelector('[role="dialog"]') as HTMLElement).querySelectorAll<HTMLElement>(
        "button, pre[tabindex]",
      ),
    ];
    expect(stops.some((el) => el.getAttribute("aria-label") === "Shell output")).toBe(true);
    const buttons = stops;
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

  it("names the output region and leaves a reader who scrolled up where they are", async () => {
    vi.useFakeTimers();
    try {
      agentsMock.mockReturnValue([]);
      shellsMock.mockReturnValue([shell()]);
      taskOutputMock.mockReset();
      const reply = (output: string) => ({ exists: true, output, truncated: false });
      taskOutputMock
        .mockResolvedValueOnce(reply("one"))
        .mockResolvedValueOnce(reply("one\ntwo"))
        .mockResolvedValue(reply("one\ntwo\nthree"));
      const view = render(<BackgroundAgentsPanel sessionId="s-1" />);
      fireEvent.click(view.getByTitle("Open command and output"));
      await act(() => vi.advanceTimersByTimeAsync(0));
      const out = view.getByRole("region", { name: "Shell output" });
      expect(out.textContent).toBe("one");
      Object.defineProperty(out, "scrollHeight", { value: 1000, configurable: true });
      Object.defineProperty(out, "clientHeight", { value: 100, configurable: true });

      await act(() => vi.advanceTimersByTimeAsync(2001));
      expect(out.textContent).toContain("two");
      expect(out.scrollTop).toBe(1000);

      out.scrollTop = 0;
      fireEvent.scroll(out);
      await act(() => vi.advanceTimersByTimeAsync(2001));
      expect(out.textContent).toContain("three");
      expect(out.scrollTop).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("tells a failed read apart from a missing file and retries a finished shell until it lands", async () => {
    vi.useFakeTimers();
    try {
      agentsMock.mockReturnValue([]);
      shellsMock.mockReturnValue([shell({ finished: true })]);
      taskOutputMock.mockReset();
      taskOutputMock
        .mockResolvedValueOnce(null)
        .mockResolvedValue({ exists: true, output: "recovered", truncated: false });
      const view = render(<BackgroundAgentsPanel sessionId="s-1" />);
      fireEvent.click(view.getByTitle("Open command and output"));
      await act(() => vi.advanceTimersByTimeAsync(0));
      expect(document.body.textContent).toContain("Could not load output.");
      expect(document.body.textContent).not.toContain("Output not available.");

      await act(() => vi.advanceTimersByTimeAsync(2001));
      expect(view.getByRole("region", { name: "Shell output" }).textContent).toBe("recovered");
      expect(document.body.textContent).not.toContain("Could not load output.");

      const calls = taskOutputMock.mock.calls.length;
      await act(() => vi.advanceTimersByTimeAsync(10_000));
      expect(taskOutputMock).toHaveBeenCalledTimes(calls);
    } finally {
      vi.useRealTimers();
    }
  });

  it("stops retrying a finished shell whose output never loads", async () => {
    vi.useFakeTimers();
    try {
      agentsMock.mockReturnValue([]);
      shellsMock.mockReturnValue([shell({ finished: true })]);
      taskOutputMock.mockReset();
      taskOutputMock.mockResolvedValue(null);
      const view = render(<BackgroundAgentsPanel sessionId="s-1" />);
      fireEvent.click(view.getByTitle("Open command and output"));
      await act(() => vi.advanceTimersByTimeAsync(30_000));
      expect(taskOutputMock).toHaveBeenCalledTimes(6);
      expect(document.body.textContent).toContain("Could not load output.");
    } finally {
      vi.useRealTimers();
    }
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

  it("offers Stop on a running agent only when the session is writable", () => {
    agentsMock.mockReturnValue([agent()]);
    const stopButtons = (c: HTMLElement) => [...c.querySelectorAll("button")].filter((b) => b.textContent === "Stop");
    const open = (c: HTMLElement) =>
      fireEvent.click([...c.querySelectorAll("button")].find((b) => b.textContent?.includes("Map backend lifecycle"))!);
    // Writable: Stop shows in the pane header and in the agent's detail view.
    const writable = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(stopButtons(writable.container)).toHaveLength(1);
    open(writable.container);
    expect(stopButtons(writable.container)).toHaveLength(1);
    writable.unmount();
    // Read-only (server read-only, trashed or archived): neither shows it.
    const locked = render(<BackgroundAgentsPanel sessionId="s-1" readOnly />);
    expect(stopButtons(locked.container)).toHaveLength(0);
    open(locked.container);
    expect(locked.container.textContent).toContain("do the thing");
    expect(stopButtons(locked.container)).toHaveLength(0);
  });

  it("lists a running agent with description, tool count, and last activity", () => {
    agentsMock.mockReturnValue([agent()]);
    const { container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(container.textContent).toContain("Background agents · 1");
    expect(container.textContent).toContain("Inline sub-agents also appear in the transcript");
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
    const { container, getByRole } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    expect(container.textContent).toContain("done");
    fireEvent.click(getByRole("button", { name: /Map backend lifecycle/ }));
    fireEvent.click(getByRole("button", { name: "Activity" }));
    for (const text of [
      "do the thing",
      "claude-opus-4-8",
      "found 12 files",
      "3 calls",
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
    // ACP stops the whole session, so Stop asks first.
    expect(fetchMock).not.toHaveBeenCalled();
    expect(getByRole("alertdialog").textContent).toContain("interrupts the main agent and all 1 running sub-agent");
    fireEvent.click(getByRole("button", { name: "Interrupt session" }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledWith("/api/sessions/s-1/acp/cancel", { method: "POST" }));
    vi.unstubAllGlobals();
  });

  it("disables both dialog buttons while the interrupt request is in flight", async () => {
    let finish: (res: Response) => void = () => {};
    const fetchMock = vi.fn(() => new Promise<Response>((resolve) => (finish = resolve)));
    vi.stubGlobal("fetch", fetchMock);
    agentsMock.mockReturnValue([agent({ status: "running" })]);
    const { getByRole, queryByRole } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    fireEvent.click(getByRole("button", { name: /stop/i }));
    fireEvent.click(getByRole("button", { name: "Interrupt session" }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    expect((getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(true);
    expect((getByRole("button", { name: "Interrupt session" }) as HTMLButtonElement).disabled).toBe(true);
    finish({ ok: true } as Response);
    await waitFor(() => expect(queryByRole("alertdialog")).toBeNull());
    vi.unstubAllGlobals();
  });

  it("closes an open Stop confirmation when the panel switches to another session", () => {
    const fetchMock = vi.fn(() => Promise.resolve({ ok: true } as Response));
    vi.stubGlobal("fetch", fetchMock);
    agentsMock.mockReturnValue([agent({ status: "running" })]);
    const view = render(<BackgroundAgentsPanel sessionId="s-1" />);
    fireEvent.click(view.getByRole("button", { name: /stop/i }));
    expect(view.getByRole("alertdialog")).toBeTruthy();
    view.rerender(<BackgroundAgentsPanel sessionId="s-2" />);
    expect(view.queryByRole("alertdialog")).toBeNull();
    expect(fetchMock).not.toHaveBeenCalled();
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

  describe("per-agent Stop", () => {
    const record = (over: Partial<SubagentRecord> = {}): SubagentRecord => ({
      session_id: "child-1",
      parent_session_id: "s-1",
      parent_tool_call_id: "task-1",
      state: "running",
      cancel_capable: true,
      ...over,
    });

    it("POSTs the child cancel route and surfaces a failure", async () => {
      const fetchMock = vi.fn(() =>
        Promise.resolve({ ok: false, status: 409, text: () => Promise.resolve("subagent cannot be cancelled") }),
      );
      vi.stubGlobal("fetch", fetchMock);
      agentsMock.mockReturnValue([agent()]);
      subagentsMock.mockReturnValue([record()]);
      const { getByRole, findByText } = render(<BackgroundAgentsPanel sessionId="s-1" />);
      fireEvent.click(getByRole("button", { name: "Stop sub-agent" }));
      expect(fetchMock).toHaveBeenCalledWith("/api/sessions/s-1/acp/subagents/child-1/cancel", { method: "POST" });
      expect(await findByText(/Could not stop subagent \(409\)/)).toBeTruthy();
      vi.unstubAllGlobals();
      subagentsMock.mockReturnValue([]);
    });

    it("lists and stops a child that has no legacy background-agent record", async () => {
      const fetchMock = vi.fn(() => Promise.resolve({ ok: true, status: 200, text: () => Promise.resolve("") }));
      vi.stubGlobal("fetch", fetchMock);
      agentsMock.mockReturnValue([]);
      subagentsMock.mockReturnValue([
        record({ parent_tool_call_id: "msg-1", title: "Explore repo" }),
        record({
          session_id: "child-2",
          parent_tool_call_id: null,
          title: "Unlinked",
          state: "idle",
          stop_reason: "end_turn",
        }),
      ]);
      const { container, getAllByRole } = render(<BackgroundAgentsPanel sessionId="s-1" />);
      expect(container.textContent).toContain("Background agents · 2");
      expect(container.textContent).toContain("Explore repo");
      expect(container.textContent).toContain("Unlinked");
      expect(container.textContent).toContain("done");
      const stops = getAllByRole("button", { name: "Stop sub-agent" });
      expect(stops).toHaveLength(1);
      fireEvent.click(stops[0]!);
      expect(fetchMock).toHaveBeenCalledWith("/api/sessions/s-1/acp/subagents/child-1/cancel", { method: "POST" });
      vi.unstubAllGlobals();
      subagentsMock.mockReturnValue([]);
    });

    it("lists a linked child once when a legacy record also covers it", () => {
      agentsMock.mockReturnValue([agent()]);
      subagentsMock.mockReturnValue([record()]);
      const { container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
      expect(container.textContent).toContain("Background agents · 1");
      subagentsMock.mockReturnValue([]);
    });

    it("is hidden without a cancel-capable record or for an inactive agent", () => {
      const cases: [BackgroundAgent, SubagentRecord[]][] = [
        [agent(), []],
        [agent(), [record({ cancel_capable: false })]],
        [agent({ status: "completed", endedAt: new Date().toISOString() }), [record()]],
      ];
      for (const [a, roster] of cases) {
        agentsMock.mockReturnValue([a]);
        subagentsMock.mockReturnValue(roster);
        const { queryByRole, unmount } = render(<BackgroundAgentsPanel sessionId="s-1" />);
        expect(queryByRole("button", { name: "Stop sub-agent" })).toBeNull();
        unmount();
      }
      subagentsMock.mockReturnValue([]);
    });

    it("labels requires_action and unknown, with a Stop on the active ones", () => {
      agentsMock.mockReturnValue([
        agent({ agentId: "w", toolCallId: "t-w", status: "requires_action" }),
        agent({ agentId: "u", toolCallId: "t-u", status: "unknown" }),
      ]);
      subagentsMock.mockReturnValue([record({ parent_tool_call_id: "t-w" })]);
      const { container, getAllByRole } = render(<BackgroundAgentsPanel sessionId="s-1" />);
      expect(container.textContent).toContain("Waiting on you");
      expect(container.textContent).toContain("Unknown");
      expect(getAllByRole("button", { name: "Stop sub-agent" })).toHaveLength(1);
      subagentsMock.mockReturnValue([]);
    });
  });

  it("opens the shared detail view with the full prompt and result, and steps between agents", () => {
    agentsMock.mockReturnValue([
      agent({
        agentId: "a1",
        toolCallId: "t1",
        status: "completed",
        endedAt: new Date().toISOString(),
        prompt: "a very long prompt that the narrow panel would clamp",
        result: "the complete final result text",
        tools: [{ name: "Bash", title: "ls", ok: true }],
      }),
      agent({ agentId: "a2", toolCallId: "t2", description: "Second one", status: "completed", endedAt: "2020-01-01" }),
    ]);
    const { getByRole, container } = render(<BackgroundAgentsPanel sessionId="s-1" />);
    fireEvent.click(getByRole("button", { name: /Map backend lifecycle/ }));
    // The same Input, Activity and Output sections the inline card shows.
    const text = container.textContent!;
    for (const part of ["Input", "Activity", "Output", "a very long prompt", "the complete final result text"]) {
      expect(text).toContain(part);
    }
    expect(text).toContain("Subagent 1 of 2");
    fireEvent.click(getByRole("button", { name: "Next subagent" }));
    expect(container.textContent).toContain("Subagent 2 of 2");
    fireEvent.click(getByRole("button", { name: /All agents/ }));
    expect(container.textContent).toContain("Background agents · 2");
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
    fireEvent.click(getByRole("button", { name: "Copy report" }));
    expect(writeClipboard).toHaveBeenLastCalledWith("done");
  });
});

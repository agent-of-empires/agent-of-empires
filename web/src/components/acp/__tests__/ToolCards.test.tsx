// @vitest-environment jsdom
//
// Coverage for the per-kind tool-card renderers and the pure
// formatDurationMs helper. Cards dispatch off tool.kind and the active
// AgentProfile; the contexts (agent profile, tool density, acp prefs,
// shiki theme) all have sensible defaults outside a provider, so most
// cards render without a wrapper. TodoGroupCard's classifier gates on
// the profile's `todos` capability, so those cases wrap in an
// AgentProfileProvider keyed to a profile that enables it.

import type { ReactNode } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, within } from "@testing-library/react";
import {
  ToolCard,
  TodoGroupCard,
  ToolGroupCard,
  SubagentCard,
  AsyncSubagentCard,
  formatDurationMs,
} from "../ToolCards";
import { BackgroundAgentsContext } from "../backgroundAgentsContext";
import { AgentProfileProvider } from "../../../lib/agentProfileContext";
import { AcpSessionContext } from "../../../lib/acpSessionContext";
import type { ActivityRow, BackgroundAgent, ToolCall, ToolOutputBlock } from "../../../lib/acpTypes";
import type { PluginUiEntry } from "../../../lib/api";

// The tool-card-badge renderer reads the plugin ui snapshot from context; mock
// it so the badge tests drive a fixed set of entries. Defaults to empty, so
// every other card test renders unchanged (no badges).
const { pluginEntriesRef } = vi.hoisted(() => ({ pluginEntriesRef: { current: [] as PluginUiEntry[] } }));
vi.mock("../../../lib/pluginUiContext", () => ({
  usePluginUiEntries: () => pluginEntriesRef.current,
}));

function toolWith(overrides: Partial<ToolCall> = {}): ToolCall {
  return {
    id: "t1",
    name: "tool",
    kind: "other",
    args_preview: "{}",
    started_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function completeRow(overrides: Partial<ActivityRow> = {}): ActivityRow {
  return {
    id: "done-t1",
    kind: "tool_complete",
    text: "",
    toolCallId: "t1",
    at: "2026-01-01T00:00:05Z",
    ...overrides,
  };
}

function errorRow(text = "boom"): ActivityRow {
  return {
    id: "err-t1",
    kind: "tool_error",
    text,
    toolCallId: "t1",
    at: "2026-01-01T00:00:05Z",
  };
}

describe("formatDurationMs", () => {
  it("renders sub-second durations in ms", () => {
    expect(formatDurationMs(0)).toBe("0 ms");
    expect(formatDurationMs(1)).toBe("1 ms");
    expect(formatDurationMs(999)).toBe("999 ms");
  });

  it("renders seconds with one decimal between 1s and a minute", () => {
    expect(formatDurationMs(1000)).toBe("1.0s");
    expect(formatDurationMs(1500)).toBe("1.5s");
    expect(formatDurationMs(59_999)).toBe("60.0s");
  });

  it("renders minutes and seconds at and above one minute", () => {
    expect(formatDurationMs(60_000)).toBe("1m 0s");
    expect(formatDurationMs(90_000)).toBe("1m 30s");
    expect(formatDurationMs(3_600_000)).toBe("60m 0s");
    expect(formatDurationMs(3_661_000)).toBe("61m 1s");
  });
});

describe("ToolCard dispatch by kind", () => {
  it("renders an execute (bash) card with the command and output", () => {
    const tool = toolWith({
      kind: "execute",
      name: "Bash",
      args_preview: JSON.stringify({ command: "ls -la", description: "list files" }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ text: "a\nb\nc" })} />);
    expect(container.textContent).toContain("bash");
    expect(container.textContent).toContain("ls -la");
    expect(container.textContent).toContain("done");
  });

  it("renders a read card with the path", () => {
    const tool = toolWith({
      kind: "read",
      name: "Read",
      args_preview: JSON.stringify({ path: "src/main.ts", offset: 1, limit: 10 }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ text: "line1\nline2" })} />);
    expect(container.textContent).toContain("read");
    expect(container.textContent).toContain("src/main.ts");
    expect(container.textContent).toContain("L1");
  });

  it("renders an edit card from the legacy old/new string args", () => {
    const tool = toolWith({
      kind: "edit",
      name: "Edit",
      args_preview: JSON.stringify({
        path: "src/app.ts",
        old_string: "const a = 1;",
        new_string: "const a = 2;",
      }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow()} />);
    expect(container.textContent).toContain("edit");
    expect(container.textContent).toContain("src/app.ts");
  });

  it("renders a write card and structured multi-file diffs", () => {
    const tool = toolWith({
      kind: "edit",
      name: "Write",
      args_preview: "{}",
      diffs: [
        { path: "a.ts", old_text: "", new_text: "x", created_at: "2026-01-01T00:00:00Z" },
        { path: "b.ts", old_text: "", new_text: "y", created_at: "2026-01-01T00:00:00Z" },
      ],
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow()} />);
    expect(container.textContent).toContain("write");
    expect(container.textContent).toContain("a.ts");
    expect(container.textContent).toContain("more");
  });

  it("renders a delete card", () => {
    const tool = toolWith({
      kind: "delete",
      name: "Delete",
      args_preview: JSON.stringify({ path: "old.txt" }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow()} />);
    expect(container.textContent).toContain("delete");
    expect(container.textContent).toContain("old.txt");
  });

  it("renders a search card with match count", () => {
    const tool = toolWith({
      kind: "search",
      name: "Grep",
      args_preview: JSON.stringify({ query: "TODO", path: "src" }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ text: "hit1\nhit2\nhit3" })} />);
    expect(container.textContent).toContain("search");
    expect(container.textContent).toContain("TODO");
    expect(container.textContent).toContain("3 matches");
  });

  it("renders a fetch card with the url", () => {
    const tool = toolWith({
      kind: "fetch",
      name: "Fetch",
      args_preview: JSON.stringify({ url: "https://example.com" }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ text: "{}" })} />);
    expect(container.textContent).toContain("fetch");
    expect(container.textContent).toContain("https://example.com");
  });

  it("renders a think card", () => {
    const tool = toolWith({ kind: "think", name: "Reasoning" });
    const { container } = render(<ToolCard tool={tool} />);
    expect(container.textContent).toContain("Reasoning");
  });

  it("renders the generic fallback for an unrecognised kind", () => {
    const tool = toolWith({
      kind: "weird",
      name: "DoThing",
      args_preview: JSON.stringify({ foo: "bar" }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ text: "out" })} />);
    expect(container.textContent).toContain("DoThing");
    expect(container.textContent).toContain("weird");
  });

  it("renders an error state with the failure text", () => {
    const tool = toolWith({ kind: "execute", name: "Bash", args_preview: JSON.stringify({ command: "false" }) });
    const { container } = render(<ToolCard tool={tool} result={errorRow("exit 1")} />);
    expect(container.textContent).toContain("failed");
  });

  it("renders a stopped status badge", () => {
    const tool = toolWith({ kind: "execute", name: "Bash", args_preview: JSON.stringify({ command: "sleep 9" }) });
    const stopped: ActivityRow = {
      id: "stop-t1",
      kind: "tool_stopped",
      text: "",
      toolCallId: "t1",
      at: "2026-01-01T00:00:05Z",
    };
    const { container } = render(<ToolCard tool={tool} result={stopped} />);
    expect(container.textContent).toContain("stopped");
  });

  it("renders a still-running tool with the running badge", () => {
    const tool = toolWith({ kind: "read", name: "Read", args_preview: JSON.stringify({ path: "x.ts" }) });
    const { container } = render(<ToolCard tool={tool} />);
    expect(container.textContent).toContain("running");
  });

  it("renders an MCP card from the mcp__ name convention", () => {
    const tool = toolWith({
      kind: "other",
      name: "mcp__sentry__search_issues",
      args_preview: JSON.stringify({ query: "errors" }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ text: "result" })} />);
    expect(container.textContent).toContain("MCP");
    expect(container.textContent.toLowerCase()).toContain("sentry");
  });

  it("renders the dedicated memory-recall card in recall mode", () => {
    const tool = toolWith({
      kind: "other",
      name: "memory_recall",
      memory_recall: { mode: "recall", paths: ["/home/u/.claude/memory/a.md"] },
    });
    const { getByTestId, getByRole, container } = render(<ToolCard tool={tool} result={completeRow()} />);
    expect(container.textContent).toContain("Memory recall");
    expect(container.textContent).toContain("Recalled");
    // body is folded by default; expand to reveal the path list
    fireEvent.click(getByRole("button"));
    expect(within(getByTestId("memory-recall-paths")).getByText(/a\.md/)).toBeTruthy();
  });

  it("renders the memory-recall card in synthesize mode", () => {
    const tool = toolWith({
      kind: "other",
      name: "memory_recall",
      memory_recall: { mode: "synthesize", synthesized_text: "you like tabs" },
    });
    const { getByTestId, getByRole } = render(<ToolCard tool={tool} result={completeRow()} />);
    fireEvent.click(getByRole("button"));
    expect(getByTestId("memory-recall-synthesized").textContent).toContain("you like tabs");
  });

  it("wraps a sub-agent child tool in the indented subagent frame", () => {
    const tool = toolWith({
      kind: "read",
      name: "Read",
      args_preview: JSON.stringify({
        path: "child.ts",
        _aoe_parent_tool_call_id: "parent-1",
      }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow()} />);
    expect(container.textContent).toContain("subagent");
    expect(container.textContent).toContain("child.ts");
  });

  it("does not double-wrap a nested child", () => {
    const tool = toolWith({
      kind: "read",
      name: "Read",
      args_preview: JSON.stringify({
        path: "child.ts",
        _aoe_parent_tool_call_id: "parent-1",
      }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow()} nested />);
    // nested suppresses the "↳ subagent" frame label
    expect(container.textContent).not.toContain("subagent");
    expect(container.textContent).toContain("child.ts");
  });
});

describe("ToolCard structured output media", () => {
  it("renders a text output block below the card", () => {
    const blocks: ToolOutputBlock[] = [{ kind: "text", text: "structured text" }];
    const tool = toolWith({ kind: "fetch", name: "Fetch", args_preview: JSON.stringify({ url: "https://x.io" }) });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ output: blocks })} />);
    expect(container.textContent).toContain("structured text");
  });

  it("renders an inline image from base64 data", () => {
    const blocks: ToolOutputBlock[] = [{ kind: "image", mime_type: "image/png", data: "AAAA" }];
    const tool = toolWith({ kind: "fetch", name: "Fetch", args_preview: JSON.stringify({ url: "https://x.io" }) });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ output: blocks })} />);
    const img = container.querySelector("img");
    expect(img).toBeTruthy();
    expect(img?.getAttribute("src")).toBe("data:image/png;base64,AAAA");
  });

  it("renders a resource_link as a safe anchor", () => {
    const blocks: ToolOutputBlock[] = [{ kind: "resource_link", uri: "https://example.com/doc", name: "Doc" }];
    const tool = toolWith({ kind: "fetch", name: "Fetch", args_preview: JSON.stringify({ url: "https://x.io" }) });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ output: blocks })} />);
    const link = container.querySelector("a");
    expect(link?.getAttribute("href")).toBe("https://example.com/doc");
    expect(container.textContent).toContain("Doc");
  });

  it("degrades a media block with no data and no usable uri to a placeholder", () => {
    const blocks: ToolOutputBlock[] = [{ kind: "image", mime_type: "image/png", uri: "javascript:alert(1)" }];
    const tool = toolWith({ kind: "fetch", name: "Fetch", args_preview: JSON.stringify({ url: "https://x.io" }) });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ output: blocks })} />);
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain("image (image/png)");
  });

  it("renders an inline audio player from base64 data", () => {
    const blocks: ToolOutputBlock[] = [{ kind: "audio", mime_type: "audio/mpeg", data: "QUJD" }];
    const tool = toolWith({ kind: "fetch", name: "Fetch", args_preview: JSON.stringify({ url: "https://x.io" }) });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ output: blocks })} />);
    const audio = container.querySelector("audio");
    expect(audio?.getAttribute("src")).toBe("data:audio/mpeg;base64,QUJD");
  });

  it("degrades an audio block with no data to a placeholder", () => {
    const blocks: ToolOutputBlock[] = [{ kind: "audio", mime_type: "audio/wav" }];
    const tool = toolWith({ kind: "fetch", name: "Fetch", args_preview: JSON.stringify({ url: "https://x.io" }) });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ output: blocks })} />);
    expect(container.querySelector("audio")).toBeNull();
    expect(container.textContent).toContain("audio (audio/wav)");
  });

  it("renders a text resource block as a pre", () => {
    const blocks: ToolOutputBlock[] = [{ kind: "resource", uri: "file:///x.txt", text: "resource body" }];
    const tool = toolWith({ kind: "fetch", name: "Fetch", args_preview: JSON.stringify({ url: "https://x.io" }) });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ output: blocks })} />);
    expect(container.textContent).toContain("resource body");
  });

  it("renders a binary resource block as a download link", () => {
    const blocks: ToolOutputBlock[] = [
      { kind: "resource", uri: "https://example.com/data.bin", mime_type: "application/octet-stream", data: "QUJD" },
    ];
    const tool = toolWith({ kind: "fetch", name: "Fetch", args_preview: JSON.stringify({ url: "https://x.io" }) });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ output: blocks })} />);
    const link = container.querySelector("a[download]");
    expect(link).toBeTruthy();
    expect(link?.getAttribute("download")).toBe("data.bin");
  });
});

describe("TodoGroupCard", () => {
  function todoTool(id: string, todos: Array<{ content: string; status: string }>): ToolCall {
    return toolWith({
      id,
      name: "TodoWrite",
      kind: "other",
      args_preview: JSON.stringify({ todos }),
    });
  }

  it("folds a run of todo snapshots and shows the latest list", () => {
    const items = [
      {
        tool: todoTool("a", [{ content: "step one", status: "completed" }]),
        result: completeRow({ id: "done-a", toolCallId: "a" }),
      },
      {
        tool: todoTool("b", [
          { content: "step one", status: "completed" },
          { content: "step two", status: "in_progress" },
        ]),
        result: completeRow({ id: "done-b", toolCallId: "b" }),
      },
    ];
    const { container } = render(
      <AgentProfileProvider toolKey="claude">
        <TodoGroupCard items={items} />
      </AgentProfileProvider>,
    );
    expect(container.textContent).toContain("updated 2 times");
    expect(container.textContent).toContain("step two");
    expect(container.textContent).toContain("active");
  });

  it("expands to reveal each per-snapshot TodoUpdateCard", () => {
    const items = [
      {
        tool: todoTool("a", [
          { content: "first task", status: "pending" },
          { content: "cancelled task", status: "cancelled" },
        ]),
        result: completeRow({ id: "done-a", toolCallId: "a" }),
      },
    ];
    const { container, getAllByRole } = render(
      <AgentProfileProvider toolKey="claude">
        <TodoGroupCard items={items} />
      </AgentProfileProvider>,
    );
    // expand the group header to render the inner TodoUpdateCard(s)
    fireEvent.click(getAllByRole("button")[0]);
    expect(container.textContent).toContain("2 items");
    expect(container.textContent).toContain("cancelled task");
  });

  it("returns null when no item classifies as a todo write", () => {
    const items = [{ tool: toolWith({ id: "x", name: "Bash", kind: "execute" }), result: completeRow() }];
    const { container } = render(
      <AgentProfileProvider toolKey="claude">
        <TodoGroupCard items={items} />
      </AgentProfileProvider>,
    );
    expect(container.textContent).toBe("");
  });
});

describe("ToolGroupCard", () => {
  it("summarises a run of tool calls with per-kind counts", () => {
    const items = [
      {
        tool: toolWith({ id: "g1", name: "Bash", kind: "execute", args_preview: JSON.stringify({ command: "ls" }) }),
        result: completeRow({ id: "done-g1", toolCallId: "g1", text: "out" }),
        kind: "execute",
      },
      {
        tool: toolWith({ id: "g2", name: "Read", kind: "read", args_preview: JSON.stringify({ path: "a.ts" }) }),
        result: completeRow({ id: "done-g2", toolCallId: "g2", text: "x" }),
        kind: "read",
      },
      {
        tool: toolWith({ id: "g3", name: "Read", kind: "read", args_preview: JSON.stringify({ path: "b.ts" }) }),
        result: errorRow(),
        kind: "read",
      },
    ];
    const { container } = render(<ToolGroupCard items={items} />);
    expect(container.textContent).toContain("3 actions");
    expect(container.textContent).toContain("Read 2");
    expect(container.textContent).toContain("Bash 1");
    expect(container.textContent).toContain("1 error");
  });

  it("returns null for an empty item list", () => {
    const { container } = render(<ToolGroupCard items={[]} />);
    expect(container.textContent).toBe("");
  });

  it("summarises delete / fetch / think kinds and expands to render children", () => {
    const items = [
      {
        tool: toolWith({ id: "d1", name: "Delete", kind: "delete", args_preview: JSON.stringify({ path: "z.ts" }) }),
        result: completeRow({ id: "done-d1", toolCallId: "d1" }),
        kind: "delete",
      },
      {
        tool: toolWith({ id: "f1", name: "Fetch", kind: "fetch", args_preview: JSON.stringify({ url: "https://y" }) }),
        result: completeRow({ id: "done-f1", toolCallId: "f1", text: "{}" }),
        kind: "fetch",
      },
      {
        tool: toolWith({ id: "th1", name: "Think", kind: "think" }),
        result: completeRow({ id: "done-th1", toolCallId: "th1" }),
        kind: "think",
      },
      {
        tool: toolWith({ id: "o1", name: "Custom", kind: "switch_mode" }),
        result: completeRow({ id: "done-o1", toolCallId: "o1" }),
        kind: "switch_mode",
      },
    ];
    const { container, getAllByRole } = render(<ToolGroupCard items={items} />);
    expect(container.textContent).toContain("Delete 1");
    expect(container.textContent).toContain("Fetch 1");
    expect(container.textContent).toContain("Think 1");
    // unknown kind is title-cased by labelForKind's default arm
    expect(container.textContent).toContain("Switch_mode 1");
    fireEvent.click(getAllByRole("button")[0]);
    expect(container.textContent).toContain("z.ts");
  });
});

describe("SubagentCard", () => {
  it("renders the parent task description, child count, and children when expanded", () => {
    const tool = toolWith({
      id: "task-1",
      name: "Task",
      kind: "other",
      args_preview: JSON.stringify({ description: "investigate bug" }),
    });
    const children = [
      {
        tool: toolWith({ id: "c1", name: "Read", kind: "read", args_preview: JSON.stringify({ path: "x.ts" }) }),
        result: completeRow({ id: "done-c1", toolCallId: "c1", text: "y" }),
      },
    ];
    const { container, getByRole } = render(
      <SubagentCard
        tool={tool}
        result={completeRow({ id: "done-task-1", toolCallId: "task-1" })}
        children={children}
      />,
    );
    expect(container.textContent).toContain("subagent");
    expect(container.textContent).toContain("investigate bug");
    expect(container.textContent).toContain("1 tool");
    // expand to reveal the child
    fireEvent.click(getByRole("button"));
    expect(container.textContent).toContain("x.ts");
  });

  it("shows the empty-children placeholder when expanded with no children", () => {
    const tool = toolWith({
      id: "task-2",
      name: "Task",
      kind: "other",
      args_preview: JSON.stringify({ description: "empty task" }),
    });
    const { container, getByRole } = render(
      <SubagentCard tool={tool} result={completeRow({ id: "done-task-2", toolCallId: "task-2" })} children={[]} />,
    );
    expect(container.textContent).toContain("0 tools");
    fireEvent.click(getByRole("button"));
    expect(container.textContent).toContain("No tool calls recorded yet.");
  });

  it("renders an async launch as a neutral background card before any tailer event", () => {
    const tool = toolWith({
      id: "task-async",
      name: "Map backend lifecycle",
      kind: "think",
      args_preview: JSON.stringify({ description: "Map backend lifecycle" }),
    });
    // No BackgroundAgentsContext provider: the launch event hasn't been
    // reduced yet, so the card degrades to the neutral fallback.
    const { container } = render(<AsyncSubagentCard tool={tool} />);
    expect(container.textContent).toContain("subagent");
    expect(container.textContent).toContain("Map backend lifecycle");
    expect(container.textContent).toContain("runs in background");
    // The SDK launch marker / internal agent id must never reach the user.
    expect(container.textContent).not.toContain("agentId");
    expect(container.textContent).not.toContain("Async agent launched");
    expect(container.textContent).not.toContain("internal ID");
  });

  it("reflects the live background-agent record (status, tools) when present", () => {
    const tool = toolWith({
      id: "task-live",
      name: "Map backend lifecycle",
      kind: "think",
      args_preview: JSON.stringify({ description: "Map backend lifecycle" }),
    });
    const agent: BackgroundAgent = {
      agentId: "a1",
      toolCallId: "task-live",
      description: "Map backend lifecycle",
      prompt: "do it",
      model: "claude-opus-4-8",
      status: "running",
      startedAt: new Date().toISOString(),
      endedAt: null,
      toolCount: 3,
      tools: [],
      lastTool: "Read",
      lastText: "scanning files",
      result: null,
      warning: null,
    };
    const { container } = render(
      <BackgroundAgentsContext.Provider value={{ agents: [agent] }}>
        <AsyncSubagentCard tool={tool} />
      </BackgroundAgentsContext.Provider>,
    );
    expect(container.textContent).toContain("Map backend lifecycle");
    expect(container.textContent).toContain("3 tools");
    expect(container.textContent).toContain("Read");
    expect(container.textContent).not.toContain("agentId");
  });

  it("clicking the async card opens the Sub agents pane", () => {
    const tool = toolWith({
      id: "task-open",
      name: "Map backend lifecycle",
      kind: "think",
      args_preview: JSON.stringify({ description: "Map backend lifecycle" }),
    });
    const openPane = vi.fn();
    const { getByRole } = render(
      <BackgroundAgentsContext.Provider value={{ agents: [], openPane }}>
        <AsyncSubagentCard tool={tool} />
      </BackgroundAgentsContext.Provider>,
    );
    fireEvent.click(getByRole("button"));
    expect(openPane).toHaveBeenCalledTimes(1);
  });
});

function renderClaude(node: ReactNode) {
  return render(<AgentProfileProvider toolKey="claude">{node}</AgentProfileProvider>);
}

describe("SkillToolCard (claude profile)", () => {
  it("renders the skill name and its input args when expanded", () => {
    const tool = toolWith({
      kind: "other",
      name: "Skill",
      args_preview: JSON.stringify({ skill: "investigate", _aoe_title: "Skill", arg: "value" }),
    });
    const { container, getAllByRole } = renderClaude(<ToolCard tool={tool} result={completeRow({ text: "ran" })} />);
    expect(container.textContent).toContain("skill");
    expect(container.textContent).toContain("investigate");
    // expand; first button is the card header toggle
    fireEvent.click(getAllByRole("button")[0]);
    expect(container.textContent).toContain("input");
    // bookkeeping title field is stripped from the rendered input
    expect(container.textContent).not.toContain("_aoe_title");
  });
});

describe("ScheduleToolCard (claude profile)", () => {
  it("renders a scheduled wakeup with a humanised delay and reason", () => {
    const tool = toolWith({
      kind: "other",
      name: "ScheduleWakeup",
      args_preview: JSON.stringify({
        _aoe_title: "ScheduleWakeup",
        delaySeconds: 194,
        reason: "check CI",
        prompt: "x",
      }),
    });
    const { container } = renderClaude(<ToolCard tool={tool} result={completeRow()} />);
    expect(container.textContent).toContain("scheduled wakeup");
    expect(container.textContent).toContain("in 3m 14s");
    expect(container.textContent).toContain("check CI");
  });

  it("renders a cron create card with the schedule expression", () => {
    const tool = toolWith({
      kind: "other",
      name: "CronCreate",
      args_preview: JSON.stringify({ _aoe_title: "CronCreate", schedule: "0 9 * * *", reason: "daily" }),
    });
    const { container } = renderClaude(<ToolCard tool={tool} result={completeRow()} />);
    expect(container.textContent).toContain("cron schedule created");
    expect(container.textContent).toContain("0 9 * * *");
  });

  it("renders a cron list card", () => {
    const tool = toolWith({
      kind: "other",
      name: "CronList",
      args_preview: JSON.stringify({ _aoe_title: "CronList" }),
    });
    const { container } = renderClaude(<ToolCard tool={tool} result={completeRow({ text: "schedule A" })} />);
    expect(container.textContent).toContain("cron schedules");
    expect(container.textContent).toContain("list active schedules");
  });

  it("renders a cron delete card with the target id", () => {
    const tool = toolWith({
      kind: "other",
      name: "CronDelete",
      args_preview: JSON.stringify({ _aoe_title: "CronDelete", id: "job-7" }),
    });
    const { container } = renderClaude(<ToolCard tool={tool} result={completeRow()} />);
    expect(container.textContent).toContain("cron schedule deleted");
    expect(container.textContent).toContain("job-7");
  });

  it("formats a multi-day wakeup delay", () => {
    const twoDaysFourHours = 2 * 86400 + 4 * 3600;
    const tool = toolWith({
      kind: "other",
      name: "ScheduleWakeup",
      args_preview: JSON.stringify({ _aoe_title: "ScheduleWakeup", delaySeconds: twoDaysFourHours }),
    });
    const { container } = renderClaude(<ToolCard tool={tool} result={completeRow()} />);
    expect(container.textContent).toContain("in 2d 4h");
  });
});

describe("MemoryCard (path-sniff)", () => {
  it("renders a memory read with parsed frontmatter when expanded", () => {
    const tool = toolWith({
      kind: "read",
      name: "Read",
      args_preview: JSON.stringify({ path: "/home/u/.claude/projects/proj/memory/feedback_x.md" }),
    });
    const body = "---\nname: feedback x\ntype: feedback\ndescription: a note\n---\nbody text here";
    const { container, getByRole } = render(<ToolCard tool={tool} result={completeRow({ text: body })} />);
    expect(container.textContent).toContain("Memory");
    expect(container.textContent).toContain("recalled");
    expect(container.textContent).toContain("feedback_x.md");
    fireEvent.click(getByRole("button"));
    expect(container.textContent).toContain("a note");
  });

  it("labels a MEMORY.md read as the memory index", () => {
    const tool = toolWith({
      kind: "read",
      name: "Read",
      args_preview: JSON.stringify({ path: "/home/u/.claude/projects/proj/memory/MEMORY.md" }),
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ text: "index body" })} />);
    expect(container.textContent).toContain("Memory index");
    expect(container.textContent).toContain("read index");
  });
});

describe("DurationLabel", () => {
  it("shows a static elapsed label for a completed call", () => {
    const tool = toolWith({
      kind: "execute",
      name: "Bash",
      args_preview: JSON.stringify({ command: "sleep" }),
      started_at: "2026-01-01T00:00:00.000Z",
    });
    const { container } = render(<ToolCard tool={tool} result={completeRow({ at: "2026-01-01T00:00:02.500Z" })} />);
    expect(container.textContent).toContain("2.5s");
  });

  it("shows a live elapsed label while the call is still running", () => {
    const tool = toolWith({
      kind: "execute",
      name: "Bash",
      args_preview: JSON.stringify({ command: "sleep" }),
      started_at: new Date(Date.now() - 1500).toISOString(),
    });
    const { container } = render(<ToolCard tool={tool} />);
    // running card has no result; duration ticks from start to now
    expect(container.textContent).toContain("running");
    expect(/\ds/.test(container.textContent ?? "")).toBe(true);
  });
});

describe("plugin tool-card-badge on cards", () => {
  afterEach(() => {
    pluginEntriesRef.current = [];
  });

  const provenance = (target: { kind: string; name: string }, text: string): PluginUiEntry => ({
    plugin_id: "acme.prov",
    slot: "tool-card-badge",
    id: "provenance",
    session_id: "s1",
    payload: { items: [{ target, text }] },
  });

  it("shows the plugin badge on the matching MCP card", () => {
    pluginEntriesRef.current = [provenance({ kind: "mcp", name: "github" }, "Company MCP")];
    const tool = toolWith({ kind: "other", name: "mcp__github__get_issue" });
    const { container } = render(
      <AcpSessionContext.Provider value="s1">
        <ToolCard tool={tool} result={completeRow()} />
      </AcpSessionContext.Provider>,
    );
    expect(container.textContent).toContain("MCP · Github");
    expect(container.textContent).toContain("Company MCP");
  });

  it("shows the plugin badge on the matching skill card", () => {
    pluginEntriesRef.current = [provenance({ kind: "skill", name: "investigate" }, "Bundled")];
    const tool = toolWith({
      kind: "other",
      name: "Skill",
      args_preview: JSON.stringify({ skill: "investigate", _aoe_title: "Skill" }),
    });
    const { container } = render(
      <AgentProfileProvider toolKey="claude">
        <AcpSessionContext.Provider value="s1">
          <ToolCard tool={tool} result={completeRow({ text: "ran" })} />
        </AcpSessionContext.Provider>
      </AgentProfileProvider>,
    );
    expect(container.textContent).toContain("investigate");
    expect(container.textContent).toContain("Bundled");
  });

  it("does not badge a card whose target does not match", () => {
    pluginEntriesRef.current = [provenance({ kind: "mcp", name: "gitlab" }, "wrong")];
    const tool = toolWith({ kind: "other", name: "mcp__github__get_issue" });
    const { container } = render(
      <AcpSessionContext.Provider value="s1">
        <ToolCard tool={tool} result={completeRow()} />
      </AcpSessionContext.Provider>,
    );
    expect(container.textContent).not.toContain("wrong");
  });
});

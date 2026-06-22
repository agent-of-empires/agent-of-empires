// @vitest-environment jsdom
//
// Per-kind dispatch coverage for ToolCards. Today only the
// `formatDurationMs` helper has a unit test; every per-kind render
// branch (bash, read, edit, search, todo, skill, schedule, mcp,
// generic) is uncovered. This spec pins each branch to a label / DOM
// shape so a future refactor that drops or misroutes a card surfaces
// here loudly.
//
// We render via the public `<ToolCard>` dispatcher because the
// per-kind functions (ExecuteToolCard, EditToolCard, etc.) are not
// exported. Shiki is mocked away so HighlightedBlock falls through to
// a plain <pre> and the test doesn't depend on async theme loading.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import type { ReactNode } from "react";

vi.mock("../../lib/highlighter", () => ({
  ensureThemeLoaded: vi.fn().mockResolvedValue("dark-plus"),
  getHighlighter: vi.fn().mockResolvedValue({
    codeToHtml: (code: string) => `<pre>${code}</pre>`,
  }),
  langKeyForExt: (s: string) => s,
  langImportForPath: () => null,
  loadLanguage: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "dark-plus", appearance: "dark" }),
}));

import { ToolCard, TodoGroupCard } from "./ToolCards";
import { AgentProfileProvider } from "../../lib/agentProfileContext";
import { AcpFileRefContext } from "./AcpFileRefContext";
import type { FileRefSession } from "../../lib/fileRef";
import { fixtures, makeCompletion, makeError, makeStopped, makeToolCall } from "./__fixtures__/toolCalls";

function Wrap({ toolKey, children }: { toolKey?: string; children: ReactNode }) {
  return <AgentProfileProvider toolKey={toolKey ?? null}>{children}</AgentProfileProvider>;
}

function WrapWithSession({ session, children }: { session: FileRefSession | null; children: ReactNode }) {
  return (
    <AcpFileRefContext.Provider value={{ fileRefSession: session }}>
      <AgentProfileProvider toolKey={null}>{children}</AgentProfileProvider>
    </AcpFileRefContext.Provider>
  );
}

afterEach(() => {
  cleanup();
});

describe("ToolCards dispatch", () => {
  it("renders bash kind with a 'bash' label and the command", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("bash");
    expect(container.textContent).toContain("ls -la");
  });

  it("renders read kind with a 'read' label and the file path", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.read} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("read");
    expect(container.textContent).toContain("/tmp/main.rs");
  });

  it("renders edit kind with an 'edit' label when old_string is non-empty", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.edit} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("edit");
    expect(container.textContent).toContain("/tmp/main.rs");
  });

  it("renders edit kind with a 'write' label when old_string is empty", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.write} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("write");
    expect(container.textContent).toContain("/tmp/new.rs");
  });

  it("renders a Codex structured-diff edit with its path and a diff body (not '(unknown file)')", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.codexEdit} result={undefined} />
      </Wrap>,
    );
    // Path shows in the (collapsed) header.
    expect(container.textContent).toContain("edit");
    expect(container.textContent).toContain("src/codex.rs");
    expect(container.textContent).not.toContain("(unknown file)");
    // Expand to reveal the diff body.
    fireEvent.click(container.querySelector("button")!);
    expect(container.querySelector('[data-testid="string-diff"]')).not.toBeNull();
  });

  it("renders every touched file path for a multi-file Codex patch", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.codexEditMultiFile} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("src/alpha.rs");
    expect(container.textContent).not.toContain("(unknown file)");
    // The second file's path lives in the expanded body.
    fireEvent.click(container.querySelector("button")!);
    expect(container.textContent).toContain("src/beta.rs");
    expect(container.querySelectorAll('[data-testid="string-diff"]').length).toBe(2);
  });

  it("renders delete kind with a 'delete' label", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.del} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("delete");
    expect(container.textContent).toContain("/tmp/gone.rs");
  });

  it("renders search kind with a 'search' label", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.search} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("search");
  });

  it("renders fetch kind with a 'fetch' label and the URL", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.fetch} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("fetch");
    expect(container.textContent).toContain("example.com");
  });

  it("renders generic kind for unrecognised tool names", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.generic} result={undefined} />
      </Wrap>,
    );
    // Generic falls back to tool.kind as the label.
    expect(container.textContent).toContain("WeirdTool");
  });

  it("flips the status pill to 'failed' on tool_error results", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeError({ text: "command not found" })} />
      </Wrap>,
    );
    expect(container.textContent?.toLowerCase()).toContain("failed");
  });

  it("renders the 'done' badge on tool_complete results", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeCompletion({ text: "hello\n" })} />
      </Wrap>,
    );
    expect(container.textContent).toContain("done");
  });

  it("renders the 'stopped' badge on tool_stopped results, not running/failed/done", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeStopped()} />
      </Wrap>,
    );
    const text = container.textContent ?? "";
    expect(text).toContain("stopped");
    expect(text).not.toContain("running");
    expect(text).not.toContain("failed");
    expect(text).not.toContain("done");
  });

  it("freezes the duration on a tool_stopped result (endedAt is set)", () => {
    // A stopped card carries a terminal `at`, so the duration is a fixed
    // span rather than a live-ticking elapsed timer. started_at
    // 00:00:00 -> at 00:00:01 == 1.0s. See #1646.
    const { container } = render(
      <Wrap>
        <ToolCard
          tool={makeToolCall({
            id: "bash-1",
            kind: "execute",
            started_at: "2026-05-21T00:00:00Z",
          })}
          result={makeStopped({ at: "2026-05-21T00:00:01Z" })}
        />
      </Wrap>,
    );
    expect(container.textContent).toContain("1.0s");
  });
});

describe("ToolCards profile-gated dispatch (claude)", () => {
  it("routes TodoWrite to the todos card under the claude profile", () => {
    const { container } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.todoWrite} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("todos");
    expect(container.textContent).toContain("Step one");
    expect(container.textContent).toContain("Step two");
    expect(container.textContent).toContain("Step three");
  });

  it("renders an empty TodoWrite clear as a todos card, not a bare think card (#2003)", () => {
    const { container } = render(
      <Wrap toolKey="claude">
        <ToolCard
          tool={makeToolCall({
            id: "todo-clear",
            name: "TodoWrite",
            kind: "think",
            args_preview: JSON.stringify({ todos: [] }),
          })}
          result={makeCompletion({ id: "done-clear", toolCallId: "todo-clear" })}
        />
      </Wrap>,
    );
    // Routes to the todos card (label "todos") and reads "todos cleared",
    // rather than falling through to ThinkToolCard's bare italic name.
    expect(container.textContent).toContain("todos");
    expect(container.textContent).toContain("todos cleared");
  });

  it("routes a Skill tool to the skill card under the claude profile", () => {
    const { container } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.skill} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("skill");
  });

  it("routes ScheduleWakeup to a wakeup card under the claude profile", () => {
    const { container } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.scheduleWakeup} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("checking deploy");
  });

  it("routes ToolSearch to the harness card under the claude profile", () => {
    const { container } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.toolSearch} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("tool search");
    expect(container.textContent).toContain("select:Read,Edit");
  });

  it("routes Monitor to the harness card under the claude profile", () => {
    const { container } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.monitor} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("monitor");
    expect(container.textContent).toContain("errors in deploy.log");
    expect(container.textContent).toContain("persistent");
  });

  it("routes TaskStop to the harness card under the claude profile", () => {
    const { container } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.taskStop} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("task stop");
    expect(container.textContent).toContain("task-abc123");
  });
});

describe("ToolCards harness-tool gating (non-claude)", () => {
  it("does not fire the harness card for a coincidental Monitor name under codex", () => {
    const { container } = render(
      <Wrap toolKey="codex">
        <ToolCard tool={fixtures.monitor} result={undefined} />
      </Wrap>,
    );
    // Generic fallback: header shows the raw name + "other" kind, and the
    // collapsed body keeps the description (`errors in deploy.log`) out of
    // the DOM. The harness card would have surfaced it in the header.
    expect(container.textContent).toContain("Monitor");
    expect(container.textContent).not.toContain("errors in deploy.log");
  });
});

describe("ToolCards harness-tool details", () => {
  it("shows a timeout chip and the expanded body for a non-persistent Monitor", () => {
    const tool = makeToolCall({
      id: "monitor-timeout",
      name: "Monitor",
      kind: "other",
      args_preview: JSON.stringify({
        description: "watch the build",
        command: "npm run build",
        timeout_ms: 90000,
      }),
    });
    const { container } = render(
      <Wrap toolKey="claude">
        <ToolCard
          tool={tool}
          result={makeCompletion({ id: "m-done", toolCallId: "monitor-timeout", text: "build ok" })}
        />
      </Wrap>,
    );
    // timeout_ms 90000 -> formatted as "1m 30s" in the header chip.
    expect(container.textContent).toContain("1m 30s");
    fireEvent.click(container.querySelector("button")!);
    expect(container.textContent).toContain("npm run build");
    expect(container.textContent).toContain("build ok");
  });

  it("falls back to default primaries when harness args are empty", () => {
    for (const [name, expected] of [
      ["ToolSearch", "search tools"],
      ["Monitor", "background watch"],
      ["TaskStop", "stop task"],
    ] as const) {
      const { container } = render(
        <Wrap toolKey="claude">
          <ToolCard
            tool={makeToolCall({ id: `empty-${name}`, name, kind: "other", args_preview: "{}" })}
            result={undefined}
          />
        </Wrap>,
      );
      expect(container.textContent).toContain(expected);
      cleanup();
    }
  });
});

describe("ToolCards profile-gated dispatch (opencode)", () => {
  it("routes OpenCode todowrite payloads to the todos card", () => {
    const { container } = render(
      <Wrap toolKey="opencode">
        <ToolCard
          tool={makeToolCall({
            id: "oc-todo-1",
            name: "5 todos",
            kind: "other",
            args_preview: JSON.stringify({
              todos: [
                { content: "Check ACP schema", status: "completed" },
                { content: "Render OpenCode todos", status: "in_progress" },
              ],
            }),
          })}
          result={undefined}
        />
      </Wrap>,
    );

    expect(container.textContent).toContain("todos");
    expect(container.textContent).toContain("2 items");
    expect(container.textContent).toContain("Check ACP schema");
    expect(container.textContent).toContain("Render OpenCode todos");
  });
});

describe("TodoGroupCard fold (#1468)", () => {
  function snapshot(id: string, content: string, status: string) {
    return {
      tool: makeToolCall({
        id,
        name: "TodoWrite",
        kind: "other",
        args_preview: JSON.stringify({ todos: [{ content, status }] }),
      }),
      result: makeCompletion({ id: `done-${id}`, toolCallId: id }),
    };
  }

  const items = [
    snapshot("td1", "Step Alpha", "in_progress"),
    snapshot("td2", "Step Bravo", "in_progress"),
    snapshot("td3", "Step Charlie", "in_progress"),
  ];

  const opencodeItems = [
    {
      tool: makeToolCall({
        id: "oc-td1",
        name: "todowrite",
        kind: "other",
        args_preview: JSON.stringify({
          todos: [{ content: "OpenCode Alpha", status: "pending" }],
        }),
      }),
      result: makeCompletion({ id: "done-oc-td1", toolCallId: "oc-td1" }),
    },
    {
      tool: makeToolCall({
        id: "oc-td2",
        name: "2 todos",
        kind: "other",
        args_preview: JSON.stringify({
          todos: [{ content: "OpenCode Bravo", status: "in_progress" }],
        }),
      }),
      result: makeCompletion({ id: "done-oc-td2", toolCallId: "oc-td2" }),
    },
    {
      tool: makeToolCall({
        id: "oc-td3",
        name: "1 todos",
        kind: "other",
        args_preview: JSON.stringify({
          todos: [{ content: "OpenCode Charlie", status: "completed" }],
        }),
      }),
      result: makeCompletion({ id: "done-oc-td3", toolCallId: "oc-td3" }),
    },
  ];

  it("shows the latest snapshot collapsed without expanding", () => {
    const { container } = render(
      <Wrap toolKey="claude">
        <TodoGroupCard items={items} />
      </Wrap>,
    );
    expect(container.textContent).toContain("todos");
    expect(container.textContent).toContain("updated 3 times");
    // Collapsed view shows the latest list only.
    expect(container.textContent).toContain("Step Charlie");
    expect(container.textContent).not.toContain("Step Alpha");
    expect(container.textContent).not.toContain("Step Bravo");
  });

  it("reveals every snapshot in order on expand", () => {
    const { container, getByRole } = render(
      <Wrap toolKey="claude">
        <TodoGroupCard items={items} />
      </Wrap>,
    );
    // Collapsed: only the group header carries a toggle.
    fireEvent.click(getByRole("button"));
    const text = container.textContent ?? "";
    expect(text).toContain("Step Alpha");
    expect(text).toContain("Step Bravo");
    expect(text).toContain("Step Charlie");
    // History renders each call in original order.
    expect(text.indexOf("Step Alpha")).toBeLessThan(text.indexOf("Step Bravo"));
  });

  it("folds a run ending in an empty clear into one group reading cleared (#2003)", () => {
    const clearTail = {
      tool: makeToolCall({
        id: "td-clear",
        name: "TodoWrite",
        kind: "think",
        args_preview: JSON.stringify({ todos: [] }),
      }),
      result: makeCompletion({ id: "done-td-clear", toolCallId: "td-clear" }),
    };
    const { container } = render(
      <Wrap toolKey="claude">
        <TodoGroupCard items={[...items, clearTail]} />
      </Wrap>,
    );
    // The empty clear survives classification, so it counts toward the
    // fold (4 snapshots) and the collapsed preview reads "todos cleared".
    expect(container.textContent).toContain("updated 4 times");
    expect(container.textContent).toContain("todos cleared");
    expect(container.textContent).not.toContain("Step Charlie");
  });

  it("falls back to the last successful snapshot when the latest failed", () => {
    const failedTail = {
      tool: makeToolCall({
        id: "td4",
        name: "TodoWrite",
        kind: "other",
        args_preview: JSON.stringify({
          todos: [{ content: "Broken plan", status: "in_progress" }],
        }),
      }),
      result: makeError({ id: "done-td4", toolCallId: "td4" }),
    };
    const { container } = render(
      <Wrap toolKey="claude">
        <TodoGroupCard items={[...items, failedTail]} />
      </Wrap>,
    );
    // Collapsed preview shows the last good snapshot, not the failed one.
    expect(container.textContent).toContain("Step Charlie");
    expect(container.textContent).not.toContain("Broken plan");
    // The header surfaces the failed latest attempt rather than looking clean.
    expect(container.textContent).toContain("failed");
  });

  it("surfaces a stopped header when the latest snapshot was interrupted (#1646)", () => {
    const stoppedTail = {
      tool: makeToolCall({
        id: "td4",
        name: "TodoWrite",
        kind: "other",
        args_preview: JSON.stringify({
          todos: [{ content: "Interrupted plan", status: "in_progress" }],
        }),
      }),
      result: makeStopped({ id: "stopped-td4", toolCallId: "td4" }),
    };
    const { container } = render(
      <Wrap toolKey="claude">
        <TodoGroupCard items={[...items, stoppedTail]} />
      </Wrap>,
    );
    // Collapsed preview falls back to the last good snapshot, not the
    // interrupted one.
    expect(container.textContent).toContain("Step Charlie");
    expect(container.textContent).not.toContain("Interrupted plan");
    // The header reads "stopped", not the misleading "done".
    expect(container.textContent).toContain("stopped");
    expect(container.textContent).not.toContain("done");
  });

  it("keeps OpenCode todowrite groups visible", () => {
    const { container } = render(
      <Wrap toolKey="opencode">
        <TodoGroupCard items={opencodeItems} />
      </Wrap>,
    );

    expect(container.textContent).toContain("todos");
    expect(container.textContent).toContain("updated 3 times");
    expect(container.textContent).toContain("OpenCode Charlie");
    expect(container.textContent).not.toContain("OpenCode Alpha");
    expect(container.textContent).not.toContain("OpenCode Bravo");
  });
});

describe("ToolCards memory_recall (claude-agent-acp v0.37.0)", () => {
  it("renders recall mode with the loaded memory paths after expansion", () => {
    const { container, getByRole, getByTestId } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.memoryRecallList} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("Memory recall");
    expect(container.textContent).toContain("Recalled");
    expect(container.textContent).toContain("2 memories");
    // Body renders only after the toggle is clicked (matches the
    // existing CardChrome pattern). Open it to assert the paths land.
    fireEvent.click(getByRole("button"));
    const list = getByTestId("memory-recall-paths");
    expect(list.textContent).toContain("user_role.md");
    expect(list.textContent).toContain("feedback_no_em_dashes.md");
  });

  it("renders synthesize mode as markdown with the system-reminder envelope and line numbers stripped", () => {
    const { container, getByRole, getByTestId } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.memoryRecallSynthesize} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("Memory recall");
    expect(container.textContent).toContain("Synthesised memory");
    fireEvent.click(getByRole("button"));
    const body = getByTestId("memory-recall-synthesized");
    // Body content survives.
    expect(body.textContent).toContain("User is a senior engineer working on agent-of-empires.");
    expect(body.textContent).toContain("prefers terse output");
    // Transport noise is gone: no envelope tag text, no cat -n line numbers.
    expect(body.textContent).not.toContain("system-reminder");
    expect(body.textContent).not.toMatch(/^\s*\d+\t/m);
    // Markdown rendered to elements, not raw source.
    expect(body.querySelector("h1")?.textContent).toBe("User profile");
    expect(body.querySelectorAll("li").length).toBe(2);
  });

  it("sanitizes dangerous HTML in synthesized memory before rendering", () => {
    const tool = makeToolCall({
      id: "mem-xss",
      name: "Recalled synthesized memory",
      kind: "read",
      args_preview: "{}",
      memory_recall: {
        mode: "synthesize",
        synthesized_text: 'Hi <img src=x onerror="alert(1)"> <a href="javascript:alert(2)">link</a>',
      },
    });
    const { getByRole, getByTestId } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={tool} result={undefined} />
      </Wrap>,
    );
    fireEvent.click(getByRole("button"));
    const body = getByTestId("memory-recall-synthesized");
    // DOMPurify strips the event handler and the javascript: URL.
    expect(body.querySelector("img")?.getAttribute("onerror")).toBeNull();
    expect(body.innerHTML).not.toContain("onerror");
    expect(body.innerHTML).not.toContain("javascript:");
  });
});

describe("ToolCards MCP", () => {
  it("renders an MCP card with the server name and verb", () => {
    const { container } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.mcp} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent?.toLowerCase()).toContain("mcp");
    expect(container.textContent?.toLowerCase()).toContain("slack");
    expect(container.textContent?.toLowerCase()).toContain("send message");
  });
});

// #1467: failed tool cards auto-open on failure but must stay foldable.
// Before the fix the card was hard-wired `expanded={open || status ===
// "err"}`, so the chevron rotated but never collapsed the body.
describe("ToolCards failed-card folding (#1467)", () => {
  it("renders the error body on first paint for a failed card", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeError({ text: "boom: command failed" })} />
      </Wrap>,
    );
    expect(container.textContent).toContain("tool failed");
    expect(container.textContent).toContain("boom: command failed");
  });

  it("folds the error body when the chevron is clicked", () => {
    const { container, getByRole } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeError({ text: "boom: command failed" })} />
      </Wrap>,
    );
    expect(container.textContent).toContain("tool failed");
    fireEvent.click(getByRole("button"));
    expect(container.textContent).not.toContain("tool failed");
    expect(container.textContent).not.toContain("boom: command failed");
    // Clicking again re-expands.
    fireEvent.click(getByRole("button"));
    expect(container.textContent).toContain("tool failed");
  });

  it("keeps a successful card collapsed by default", () => {
    const { container } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeCompletion({ text: "hello world\n" })} />
      </Wrap>,
    );
    // Header is present, body output is hidden until the user expands.
    expect(container.textContent).toContain("bash");
    expect(container.textContent).not.toContain("hello world");
  });

  it("auto-opens a card that fails mid-stream (running -> err)", () => {
    const { container, rerender } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={undefined} />
      </Wrap>,
    );
    // Running: no error body yet.
    expect(container.textContent).not.toContain("tool failed");
    rerender(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeError({ text: "boom: command failed" })} />
      </Wrap>,
    );
    // The error row arrives and the card opens with no user click.
    expect(container.textContent).toContain("tool failed");
    expect(container.textContent).toContain("boom: command failed");
  });

  it("respects the user's fold once set, even if the card re-enters err", () => {
    const { container, getByRole, rerender } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeError({ text: "boom: command failed" })} />
      </Wrap>,
    );
    // User folds the failed card.
    fireEvent.click(getByRole("button"));
    expect(container.textContent).not.toContain("tool failed");
    // A later render still reports err: the card stays folded.
    rerender(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeError({ text: "boom again" })} />
      </Wrap>,
    );
    expect(container.textContent).not.toContain("tool failed");
  });

  // The MemoryRecall and Schedule cards previously gated their toggle on
  // `hasBody` alone, so a failed card with no normal body had an
  // unclickable header. They now include `status === "err"` in the
  // predicate; exercise each failed-and-foldable so that branch (and the
  // shared hook call site) stays covered.
  const errToggleKinds: Array<[string, () => unknown]> = [
    ["memoryRecall", () => fixtures.memoryRecallList],
    ["scheduleWakeup", () => fixtures.scheduleWakeup],
  ];

  it.each(errToggleKinds)("auto-opens and folds a failed %s card", (_label, getTool) => {
    const { container, getAllByRole } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={getTool() as never} result={makeError({ text: "kind-specific boom" })} />
      </Wrap>,
    );
    // Auto-open on failure: the rose error block is visible with no click.
    expect(container.textContent).toContain("tool failed");
    // The header is the card's first button; clicking it folds the body.
    fireEvent.click(getAllByRole("button")[0]);
    expect(container.textContent).not.toContain("tool failed");
  });

  it("auto-opens and folds a failed memory-file card", () => {
    // A Read on a path under Claude's per-project memory dir dispatches
    // to the dedicated MemoryCard, which shares the same hook.
    const tool = makeToolCall({
      id: "mem-1",
      name: "Read",
      kind: "read",
      args_preview: JSON.stringify({
        file_path: "/Users/test/.claude/projects/foo/memory/feedback_testing.md",
      }),
    });
    const { container, getAllByRole } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={tool} result={makeError({ text: "memory read boom" })} />
      </Wrap>,
    );
    expect(container.textContent).toContain("tool failed");
    fireEvent.click(getAllByRole("button")[0]);
    expect(container.textContent).not.toContain("tool failed");
  });

  // #1818: structured completion payloads (media/resources) render below
  // the card regardless of tool kind, and are not gated behind expansion.
  it("renders an inline image from a base64 completion payload", () => {
    const { container } = render(
      <Wrap>
        <ToolCard
          tool={fixtures.generic}
          result={makeCompletion({
            output: [{ kind: "image", mime_type: "image/png", data: "BASE64IMG" }],
          })}
        />
      </Wrap>,
    );
    const img = container.querySelector("img");
    expect(img).not.toBeNull();
    expect(img!.getAttribute("src")).toBe("data:image/png;base64,BASE64IMG");
  });

  it("renders an audio player and a resource link from a completion payload", () => {
    const { container } = render(
      <Wrap>
        <ToolCard
          tool={fixtures.generic}
          result={makeCompletion({
            output: [
              { kind: "audio", mime_type: "audio/wav", data: "BASE64AUDIO" },
              {
                kind: "resource_link",
                uri: "file:///report.pdf",
                name: "report.pdf",
              },
            ],
          })}
        />
      </Wrap>,
    );
    const audio = container.querySelector("audio");
    expect(audio).not.toBeNull();
    expect(audio!.getAttribute("src")).toBe("data:audio/wav;base64,BASE64AUDIO");
    const link = container.querySelector('a[href="file:///report.pdf"]');
    expect(link).not.toBeNull();
    expect(container.textContent).toContain("report.pdf");
  });

  it("degrades a data-less image block to a labelled placeholder", () => {
    const { container } = render(
      <Wrap>
        <ToolCard
          tool={fixtures.generic}
          result={makeCompletion({
            output: [{ kind: "image", mime_type: "image/png" }],
          })}
        />
      </Wrap>,
    );
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain("image (image/png)");
  });

  it("refuses a javascript: resource link and shows a placeholder", () => {
    // #1818 review: agent-controlled uris must not reach href; a
    // javascript: scheme degrades to a non-clickable placeholder.
    const { container } = render(
      <Wrap>
        <ToolCard
          tool={fixtures.generic}
          result={makeCompletion({
            output: [
              {
                kind: "resource_link",
                uri: "javascript:alert(1)",
                name: "evil.html",
              },
            ],
          })}
        />
      </Wrap>,
    );
    expect(container.querySelector("a")).toBeNull();
    expect(container.textContent).toContain("evil.html");
  });

  it("offers a blob resource as a download from inline data", () => {
    // #1818 review: a blob resource keeps its bytes, so it downloads even
    // without a fetchable uri.
    const { container } = render(
      <Wrap>
        <ToolCard
          tool={fixtures.generic}
          result={makeCompletion({
            output: [
              {
                kind: "resource",
                uri: "file:///out.bin",
                mime_type: "application/octet-stream",
                data: "QkxPQg==",
              },
            ],
          })}
        />
      </Wrap>,
    );
    const link = container.querySelector("a[download]");
    expect(link).not.toBeNull();
    expect(link!.getAttribute("href")).toBe("data:application/octet-stream;base64,QkxPQg==");
    expect(link!.getAttribute("download")).toBe("out.bin");
  });

  it("renders a successful completion body when the card is expanded", () => {
    // Criterion 3: the success-path body wiring (result.text) renders once
    // the user opens the card, not just the error path.
    const { container, getByRole } = render(
      <Wrap>
        <ToolCard tool={fixtures.bash} result={makeCompletion({ text: "hello world\n" })} />
      </Wrap>,
    );
    expect(container.textContent).not.toContain("hello world");
    fireEvent.click(getByRole("button"));
    expect(container.textContent).toContain("hello world");
  });
});

describe("ToolCards repo-relative paths (#2143)", () => {
  const session: FileRefSession = {
    project_path: "/tmp",
    main_repo_path: null,
    workspace_repos: [],
  };

  it("renders an edit path repo-relative when it sits under the session root", () => {
    const { container } = render(
      <WrapWithSession session={session}>
        <ToolCard tool={fixtures.edit} result={undefined} />
      </WrapWithSession>,
    );
    expect(container.textContent).toContain("main.rs");
    expect(container.textContent).not.toContain("/tmp/main.rs");
  });

  it("renders a read path repo-relative under the session root", () => {
    const { container } = render(
      <WrapWithSession session={session}>
        <ToolCard tool={fixtures.read} result={undefined} />
      </WrapWithSession>,
    );
    expect(container.textContent).toContain("main.rs");
    expect(container.textContent).not.toContain("/tmp/main.rs");
  });

  it("prefixes the repo name in a multi-repo workspace", () => {
    const multi: FileRefSession = {
      project_path: "/tmp/ws",
      main_repo_path: null,
      workspace_repos: [{ name: "api", source_path: "/tmp/api" }],
    };
    const tool = makeToolCall({
      id: "edit-multi-repo",
      kind: "edit",
      args_preview: JSON.stringify({ file_path: "/tmp/api/src/h.ts", old_string: "a", new_string: "b" }),
    });
    const { container } = render(
      <WrapWithSession session={multi}>
        <ToolCard tool={tool} result={undefined} />
      </WrapWithSession>,
    );
    expect(container.textContent).toContain("api/src/h.ts");
    // The visible label must not be the absolute path; the absolute form
    // only survives in the title tooltip (an attribute, not textContent).
    expect(container.textContent).not.toContain("/tmp/api/src/h.ts");
  });

  it("falls back to the absolute path when outside every known root", () => {
    const tool = makeToolCall({
      id: "edit-outside",
      kind: "edit",
      args_preview: JSON.stringify({ file_path: "/etc/hosts", old_string: "a", new_string: "b" }),
    });
    const { container } = render(
      <WrapWithSession session={session}>
        <ToolCard tool={tool} result={undefined} />
      </WrapWithSession>,
    );
    expect(container.textContent).toContain("/etc/hosts");
  });

  it("renders a delete path repo-relative under the session root", () => {
    const { container } = render(
      <WrapWithSession session={session}>
        <ToolCard tool={fixtures.del} result={undefined} />
      </WrapWithSession>,
    );
    expect(container.textContent).toContain("delete");
    expect(container.textContent).toContain("gone.rs");
    expect(container.textContent).not.toContain("/tmp/gone.rs");
  });

  it("renders a write path repo-relative under the session root", () => {
    const { container } = render(
      <WrapWithSession session={session}>
        <ToolCard tool={fixtures.write} result={undefined} />
      </WrapWithSession>,
    );
    expect(container.textContent).toContain("new.rs");
    expect(container.textContent).not.toContain("/tmp/new.rs");
  });

  it("renders a multi-file edit with each diff header repo-relative", () => {
    const tool = makeToolCall({
      id: "edit-multi-file",
      name: "apply_patch",
      kind: "edit",
      args_preview: "{}",
      diffs: [
        { path: "/tmp/src/alpha.rs", old_text: "a", new_text: "b", created_at: "2026-05-21T00:00:00Z" },
        { path: "/tmp/src/beta.rs", old_text: null, new_text: "c", created_at: "2026-05-21T00:00:00Z" },
      ],
    });
    const { container, getByRole } = render(
      <WrapWithSession session={session}>
        <ToolCard tool={tool} result={undefined} />
      </WrapWithSession>,
    );
    // Collapsed: primary shows the first path relative + "+N more".
    expect(container.textContent).toContain("src/alpha.rs");
    expect(container.textContent).toContain("+1 more");
    expect(container.textContent).not.toContain("/tmp/src/alpha.rs");
    // Expanded: each per-file diff header renders its path relative too.
    fireEvent.click(getByRole("button"));
    expect(container.textContent).toContain("src/beta.rs");
    expect(container.textContent).not.toContain("/tmp/src/beta.rs");
  });

  it.each([
    ["read", "read"],
    ["edit", "write"],
    ["delete", "delete"],
  ])("falls back to (unknown file) for a path-less %s tool", (kind, label) => {
    const tool = makeToolCall({ id: `${kind}-no-path`, name: "", kind, args_preview: "{}" });
    const { container } = render(
      <WrapWithSession session={session}>
        <ToolCard tool={tool} result={undefined} />
      </WrapWithSession>,
    );
    expect(container.textContent).toContain(label);
    expect(container.textContent).toContain("(unknown file)");
  });

  it("keeps the absolute path in the title tooltip while showing the relative label", () => {
    const { container } = render(
      <WrapWithSession session={session}>
        <ToolCard tool={fixtures.edit} result={undefined} />
      </WrapWithSession>,
    );
    const titled = container.querySelector('[title="/tmp/main.rs"]');
    expect(titled).not.toBeNull();
    expect(titled!.textContent).toContain("main.rs");
  });
});

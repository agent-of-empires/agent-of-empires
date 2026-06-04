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
import {
  fixtures,
  makeCompletion,
  makeError,
  makeStopped,
  makeToolCall,
} from "./__fixtures__/toolCalls";

function Wrap({
  toolKey,
  children,
}: {
  toolKey?: string;
  children: ReactNode;
}) {
  return (
    <AgentProfileProvider toolKey={toolKey ?? null}>
      {children}
    </AgentProfileProvider>
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
    expect(
      container.querySelector('[data-testid="string-diff"]'),
    ).not.toBeNull();
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
    expect(
      container.querySelectorAll('[data-testid="string-diff"]').length,
    ).toBe(2);
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
        <ToolCard
          tool={fixtures.bash}
          result={makeError({ text: "command not found" })}
        />
      </Wrap>,
    );
    expect(container.textContent?.toLowerCase()).toContain("failed");
  });

  it("renders the 'done' badge on tool_complete results", () => {
    const { container } = render(
      <Wrap>
        <ToolCard
          tool={fixtures.bash}
          result={makeCompletion({ text: "hello\n" })}
        />
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

  it("renders synthesize mode with the synthesized text body after expansion", () => {
    const { container, getByRole, getByTestId } = render(
      <Wrap toolKey="claude">
        <ToolCard tool={fixtures.memoryRecallSynthesize} result={undefined} />
      </Wrap>,
    );
    expect(container.textContent).toContain("Memory recall");
    expect(container.textContent).toContain("Synthesised memory");
    fireEvent.click(getByRole("button"));
    const body = getByTestId("memory-recall-synthesized");
    expect(body.textContent).toContain(
      "User is a senior engineer working on agent-of-empires.",
    );
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
        <ToolCard
          tool={fixtures.bash}
          result={makeError({ text: "boom: command failed" })}
        />
      </Wrap>,
    );
    expect(container.textContent).toContain("tool failed");
    expect(container.textContent).toContain("boom: command failed");
  });

  it("folds the error body when the chevron is clicked", () => {
    const { container, getByRole } = render(
      <Wrap>
        <ToolCard
          tool={fixtures.bash}
          result={makeError({ text: "boom: command failed" })}
        />
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
        <ToolCard
          tool={fixtures.bash}
          result={makeCompletion({ text: "hello world\n" })}
        />
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
        <ToolCard
          tool={fixtures.bash}
          result={makeError({ text: "boom: command failed" })}
        />
      </Wrap>,
    );
    // The error row arrives and the card opens with no user click.
    expect(container.textContent).toContain("tool failed");
    expect(container.textContent).toContain("boom: command failed");
  });

  it("respects the user's fold once set, even if the card re-enters err", () => {
    const { container, getByRole, rerender } = render(
      <Wrap>
        <ToolCard
          tool={fixtures.bash}
          result={makeError({ text: "boom: command failed" })}
        />
      </Wrap>,
    );
    // User folds the failed card.
    fireEvent.click(getByRole("button"));
    expect(container.textContent).not.toContain("tool failed");
    // A later render still reports err: the card stays folded.
    rerender(
      <Wrap>
        <ToolCard
          tool={fixtures.bash}
          result={makeError({ text: "boom again" })}
        />
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

  it.each(errToggleKinds)(
    "auto-opens and folds a failed %s card",
    (_label, getTool) => {
      const { container, getAllByRole } = render(
        <Wrap toolKey="claude">
          <ToolCard
            tool={getTool() as never}
            result={makeError({ text: "kind-specific boom" })}
          />
        </Wrap>,
      );
      // Auto-open on failure: the rose error block is visible with no click.
      expect(container.textContent).toContain("tool failed");
      // The header is the card's first button; clicking it folds the body.
      fireEvent.click(getAllByRole("button")[0]);
      expect(container.textContent).not.toContain("tool failed");
    },
  );

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
});

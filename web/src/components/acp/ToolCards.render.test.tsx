// @vitest-environment jsdom
// Shiki is mocked so HighlightedBlock renders a plain <pre> synchronously.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

vi.mock("../../lib/snippetHighlighter", () => ({
  highlightSnippet: vi.fn().mockResolvedValue(null),
  getSnippetHighlighter: vi.fn().mockResolvedValue(null),
  langHintForPath: () => "",
}));

vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "dark-plus", appearance: "dark" }),
}));

const { skillIndexRef } = vi.hoisted(() => ({
  skillIndexRef: { current: { labelsByKey: new Map<string, Set<string>>() } as SkillIndex },
}));
vi.mock("../../hooks/useSkillIndex", () => ({
  useSkillIndex: () => skillIndexRef.current,
}));

import type { ActivityRow, BackgroundAgent, ToolCall, ToolOutputBlock } from "../../lib/acpTypes";
import { highlightSnippet } from "../../lib/snippetHighlighter";
import { AgentProfileProvider } from "../../lib/agentProfileContext";
import type { FileRef, FileRefSession } from "../../lib/fileRef";
import { buildSkillIndex, type SkillIndex } from "../../lib/skillProvenance";
import { AcpFileRefContext } from "./AcpFileRefContext";
import { BackgroundAgentsContext } from "./backgroundAgentsContext";
import { AsyncSubagentCard, extractTaskResult, SubagentCard, ToolGroupCard } from "./GroupToolCards";
import { TodoGroupCard } from "./TodoCards";
import { formatDurationMs, formatDurationSeconds } from "./ToolCardChrome";
import { ToolCard } from "./ToolCards";
import { fixtures, makeCompletion, makeError, makeStopped, makeToolCall } from "./__fixtures__/toolCalls";
import { renderWithLateResolution } from "../../__tests__/lateResolution";

afterEach(() => {
  cleanup();
  skillIndexRef.current = { labelsByKey: new Map() };
});

interface RenderOpts {
  toolKey?: string | null;
  session?: FileRefSession | null;
  onOpenFileRef?: (ref: FileRef) => void;
  nested?: boolean;
}

function wrap(node: React.ReactNode, { toolKey = null, session = null, onOpenFileRef }: RenderOpts = {}) {
  return (
    <AcpFileRefContext.Provider value={{ fileRefSession: session, onOpenFileRef }}>
      <AgentProfileProvider toolKey={toolKey}>{node}</AgentProfileProvider>
    </AcpFileRefContext.Provider>
  );
}

function renderCard(tool: ToolCall, result?: ActivityRow, opts: RenderOpts = {}) {
  const rendered = render(wrap(<ToolCard tool={tool} result={result} nested={opts.nested} />, opts));
  const text = () => rendered.container.textContent ?? "";
  const toggle = () => fireEvent.click(rendered.container.querySelector("button")!);
  return { ...rendered, text, toggle };
}

const args = (a: Record<string, unknown>) => JSON.stringify(a);
const MEMORY_DIR = "/home/u/.claude/projects/proj/memory";

describe("ToolCard headers", () => {
  it.each<[string, ToolCall, ActivityRow | undefined, RenderOpts, string[], string[]]>([
    ["bash", fixtures.bash, makeCompletion({ text: "a\nb" }), {}, ["bash", "ls -la", "done"], []],
    [
      "read with range",
      makeToolCall({ kind: "read", args_preview: args({ path: "src/main.ts", offset: 1, limit: 10 }) }),
      undefined,
      {},
      ["read", "src/main.ts", "L1–11", "running"],
      [],
    ],
    ["edit", fixtures.edit, undefined, {}, ["edit", "/tmp/main.rs"], []],
    ["write", fixtures.write, undefined, {}, ["write", "/tmp/new.rs"], []],
    ["codex structured diff", fixtures.codexEdit, undefined, {}, ["edit", "src/codex.rs"], ["(unknown file)"]],
    [
      "codex multi-file diff",
      fixtures.codexEditMultiFile,
      undefined,
      {},
      ["src/alpha.rs", "+1 more"],
      ["(unknown file)"],
    ],
    ["delete", fixtures.del, undefined, {}, ["delete", "/tmp/gone.rs"], []],
    [
      "search",
      makeToolCall({ kind: "search", args_preview: args({ query: "TODO", path: "src" }) }),
      makeCompletion({ text: "hit1\nhit2\nhit3" }),
      {},
      ["search", "TODO", "in src", "3 matches"],
      [],
    ],
    ["fetch", fixtures.fetch, undefined, {}, ["fetch", "example.com"], []],
    ["think", makeToolCall({ kind: "think", name: "Reasoning" }), undefined, {}, ["Reasoning"], ["running"]],
    [
      "generic",
      makeToolCall({ kind: "weird", name: "DoThing" }),
      makeCompletion({ text: "out" }),
      {},
      ["weird", "DoThing"],
      [],
    ],
    // A failed think's error is its whole content, so it routes to the generic card.
    [
      "failed think",
      makeToolCall({ kind: "think", name: "task" }),
      makeError({ text: "AI_NoSuchToolError: unavailable tool 'task'. Available tools: Read, Write, Bash." }),
      {},
      ["failed", "unavailable tool 'task'", "Available tools: Read, Write, Bash."],
      [],
    ],
    ["stopped", fixtures.bash, makeStopped(), {}, ["stopped"], ["running", "failed", "done"]],
    [
      "stopped duration is frozen",
      makeToolCall({ kind: "execute", started_at: "2026-05-21T00:00:00Z" }),
      makeStopped({ at: "2026-05-21T00:00:01Z" }),
      {},
      ["1.0s"],
      [],
    ],
    [
      "completed duration",
      makeToolCall({ kind: "execute", started_at: "2026-01-01T00:00:00.000Z" }),
      makeCompletion({ at: "2026-01-01T00:00:02.500Z" }),
      {},
      ["2.5s"],
      [],
    ],
    ["mcp", fixtures.mcp, undefined, { toolKey: "claude" }, ["MCP", "Slack", "Send message"], []],
    [
      "memory file",
      makeToolCall({ kind: "read", args_preview: args({ path: `${MEMORY_DIR}/feedback_x.md` }) }),
      makeCompletion({ text: "body" }),
      {},
      ["Memory", "recalled", "feedback_x.md"],
      [],
    ],
    [
      "memory index",
      makeToolCall({ kind: "read", args_preview: args({ path: `${MEMORY_DIR}/MEMORY.md` }) }),
      makeCompletion({ text: "index body" }),
      {},
      ["Memory index", "read index"],
      [],
    ],
    ["memory recall", fixtures.memoryRecallList, undefined, {}, ["Memory recall", "Recalled", "2 memories"], []],
    ["memory synthesize", fixtures.memoryRecallSynthesize, undefined, {}, ["Memory recall", "Synthesised memory"], []],
    [
      "subagent child",
      makeToolCall({ kind: "read", args_preview: args({ path: "child.ts", _aoe_parent_tool_call_id: "p" }) }),
      undefined,
      {},
      ["subagent", "child.ts"],
      [],
    ],
    [
      "nested subagent child",
      makeToolCall({ kind: "read", args_preview: args({ path: "child.ts", _aoe_parent_tool_call_id: "p" }) }),
      undefined,
      { nested: true },
      ["child.ts"],
      ["subagent"],
    ],
    ["claude todos", fixtures.todoWrite, undefined, { toolKey: "claude" }, ["todos", "Step one", "Step three"], []],
    // An empty todos array is a real clear, not a bare think card.
    [
      "claude todos clear",
      makeToolCall({ name: "TodoWrite", kind: "think", args_preview: args({ todos: [] }) }),
      makeCompletion(),
      { toolKey: "claude" },
      ["todos", "todos cleared"],
      [],
    ],
    [
      "opencode todos",
      makeToolCall({
        name: "5 todos",
        args_preview: args({
          todos: [
            { content: "Check ACP schema", status: "completed" },
            { content: "Render OpenCode todos", status: "in_progress" },
          ],
        }),
      }),
      undefined,
      { toolKey: "opencode" },
      ["todos", "2 items", "Check ACP schema", "Render OpenCode todos"],
      [],
    ],
    ["claude skill", fixtures.skill, undefined, { toolKey: "claude" }, ["skill", "investigate"], []],
    ["wakeup", fixtures.scheduleWakeup, undefined, { toolKey: "claude" }, ["checking deploy", "in 5m"], []],
    [
      "wakeup with humanised delay",
      makeToolCall({
        name: "ScheduleWakeup",
        args_preview: args({ delaySeconds: 194, reason: "check CI", prompt: "x" }),
      }),
      makeCompletion(),
      { toolKey: "claude" },
      ["scheduled wakeup", "in 3m 14s", "check CI"],
      [],
    ],
    [
      "cron create",
      makeToolCall({ name: "CronCreate", args_preview: args({ _aoe_title: "CronCreate", schedule: "0 9 * * *" }) }),
      makeCompletion(),
      { toolKey: "claude" },
      ["cron schedule created", "0 9 * * *"],
      [],
    ],
    [
      "cron list",
      makeToolCall({ name: "CronList" }),
      makeCompletion({ text: "schedule A" }),
      { toolKey: "claude" },
      ["cron schedules", "list active schedules"],
      [],
    ],
    [
      "cron delete",
      makeToolCall({ name: "CronDelete", args_preview: args({ id: "job-7" }) }),
      makeCompletion(),
      { toolKey: "claude" },
      ["cron schedule deleted", "job-7"],
      [],
    ],
    ["tool search", fixtures.toolSearch, undefined, { toolKey: "claude" }, ["tool search", "select:Read,Edit"], []],
    [
      "monitor",
      fixtures.monitor,
      undefined,
      { toolKey: "claude" },
      ["monitor", "errors in deploy.log", "persistent"],
      [],
    ],
    ["task stop", fixtures.taskStop, undefined, { toolKey: "claude" }, ["task stop", "task-abc123"], []],
    ["empty tool search", makeToolCall({ name: "ToolSearch" }), undefined, { toolKey: "claude" }, ["search tools"], []],
    ["empty monitor", makeToolCall({ name: "Monitor" }), undefined, { toolKey: "claude" }, ["background watch"], []],
    ["empty task stop", makeToolCall({ name: "TaskStop" }), undefined, { toolKey: "claude" }, ["stop task"], []],
    // Harness names are Claude-only; a coincidental Monitor elsewhere is generic.
    ["codex Monitor", fixtures.monitor, undefined, { toolKey: "codex" }, ["Monitor"], ["errors in deploy.log"]],
  ])("%s", (_label, tool, result, opts, contains, excludes) => {
    const { text } = renderCard(tool, result, opts);
    for (const s of contains) expect(text()).toContain(s);
    for (const s of excludes) expect(text()).not.toContain(s);
  });

  it("ticks a live duration while running", () => {
    const { text } = renderCard(
      makeToolCall({ kind: "execute", started_at: new Date(Date.now() - 1500).toISOString() }),
    );
    expect(text()).toContain("running");
    expect(text()).toMatch(/\ds/);
  });

  it("renders the resolved skill's provenance badge", () => {
    skillIndexRef.current = buildSkillIndex({
      roots: [
        { id: "claude-user", label: "Claude", relativePath: ".claude/skills", consumers: ["claude"], legacy: false },
      ],
      skills: [
        {
          directory: "investigate",
          name: "investigate",
          description: "",
          provenance: { kind: "external", root: "claude-user" },
          provenanceLabel: "external:claude-user",
          writable: false,
        },
      ],
    });
    expect(renderCard(fixtures.skill, makeCompletion({ text: "ran" }), { toolKey: "claude" }).text()).toContain(
      "Claude",
    );
  });
});

describe("ToolCard expanded bodies", () => {
  it.each<[string, ToolCall, ActivityRow | undefined, RenderOpts, string[], string[]]>([
    ["bash output", fixtures.bash, makeCompletion({ text: "hello world\n" }), {}, ["hello world"], []],
    ["codex second file", fixtures.codexEditMultiFile, undefined, {}, ["src/beta.rs"], []],
    [
      "monitor timeout chip and body",
      makeToolCall({
        name: "Monitor",
        args_preview: args({ description: "watch", command: "npm run build", timeout_ms: 90000 }),
      }),
      makeCompletion({ text: "build ok" }),
      { toolKey: "claude" },
      ["1m 30s", "npm run build", "build ok"],
      [],
    ],
    [
      "skill input without bookkeeping",
      makeToolCall({ name: "Skill", args_preview: args({ skill: "investigate", _aoe_title: "Skill", arg: "value" }) }),
      makeCompletion({ text: "ran" }),
      { toolKey: "claude" },
      ["input", "value"],
      ["_aoe_title"],
    ],
    [
      "memory frontmatter",
      makeToolCall({ kind: "read", args_preview: args({ path: `${MEMORY_DIR}/feedback_x.md` }) }),
      makeCompletion({ text: "---\nname: feedback x\ntype: feedback\ndescription: a note\n---\nbody text here" }),
      {},
      ["a note", "body text here"],
      [],
    ],
  ])("%s", (_label, tool, result, opts, contains, excludes) => {
    const { text, toggle } = renderCard(tool, result, opts);
    toggle();
    for (const s of contains) expect(text()).toContain(s);
    for (const s of excludes) expect(text()).not.toContain(s);
  });

  it("keeps a successful card collapsed until toggled", () => {
    const { text } = renderCard(fixtures.bash, makeCompletion({ text: "hello world\n" }));
    expect(text()).toContain("bash");
    expect(text()).not.toContain("hello world");
  });

  it.each([
    [fixtures.codexEdit, 1],
    [fixtures.codexEditMultiFile, 2],
  ])("renders structured diff bodies (%#)", (tool, count) => {
    const { container, toggle } = renderCard(tool);
    toggle();
    expect(container.querySelectorAll('[data-testid="string-diff"]').length).toBe(count);
  });

  it("lists recalled memory paths", () => {
    const { getByTestId, toggle } = renderCard(fixtures.memoryRecallList);
    toggle();
    const list = getByTestId("memory-recall-paths");
    expect(list.textContent).toContain("user_role.md");
    expect(list.textContent).toContain("feedback_no_em_dashes.md");
  });

  it("renders synthesized memory as markdown with the envelope and line numbers stripped", () => {
    const { getByTestId, toggle } = renderCard(fixtures.memoryRecallSynthesize);
    toggle();
    const body = getByTestId("memory-recall-synthesized");
    expect(body.textContent).toContain("User is a senior engineer working on agent-of-empires.");
    expect(body.textContent).not.toContain("system-reminder");
    expect(body.textContent).not.toMatch(/^\s*\d+\t/m);
    expect(body.querySelector("h1")?.textContent).toBe("User profile");
    expect(body.querySelectorAll("li").length).toBe(2);
  });

  it("sanitizes dangerous HTML in synthesized memory", () => {
    const tool = makeToolCall({
      memory_recall: {
        mode: "synthesize",
        synthesized_text: 'Hi <img src=x onerror="alert(1)"> <a href="javascript:alert(2)">link</a>',
      },
    });
    const { getByTestId, toggle } = renderCard(tool);
    toggle();
    const body = getByTestId("memory-recall-synthesized");
    expect(body.querySelector("img")?.getAttribute("onerror")).toBeNull();
    expect(body.innerHTML).not.toContain("onerror");
    expect(body.innerHTML).not.toContain("javascript:");
  });

  it.each([
    // An OSC 8 hyperlink with no color codes must still take the ANSI path.
    [
      "See \x1b]8;;https://example.com/pr/8\x1b\\the PR\x1b]8;;\x1b\\ now",
      "See the PR now",
      "https://example.com/pr/8",
    ],
    ["run \x1b]8;;javascript:alert(1)\x1b\\this\x1b]8;;\x1b\\ now", "run this now", null],
  ])("renders hyperlinks in output against the scheme allowlist (%#)", (output, visible, href) => {
    const { container, text, toggle } = renderCard(fixtures.bash, makeCompletion({ text: output }));
    toggle();
    expect(text()).toContain(visible);
    expect(text()).not.toContain("]8;;");
    const links = container.querySelectorAll("a");
    expect(links.length).toBe(href ? 1 : 0);
    if (href) {
      expect(links[0]!.getAttribute("href")).toBe(href);
      expect(links[0]!.textContent).toBe("the PR");
    }
  });
});

describe("failed-card folding", () => {
  it("opens a failed card on first paint and folds and reopens on toggle", () => {
    const { text, toggle } = renderCard(fixtures.bash, makeError({ text: "boom: command failed" }));
    expect(text()).toContain("tool failed");
    expect(text()).toContain("boom: command failed");
    toggle();
    expect(text()).not.toContain("tool failed");
    toggle();
    expect(text()).toContain("tool failed");
  });

  it("auto-opens a card that fails mid-stream, then respects the user's fold", () => {
    const { text, toggle, rerender } = renderCard(fixtures.bash);
    expect(text()).not.toContain("tool failed");
    rerender(wrap(<ToolCard tool={fixtures.bash} result={makeError({ text: "boom" })} />));
    expect(text()).toContain("tool failed");
    toggle();
    rerender(wrap(<ToolCard tool={fixtures.bash} result={makeError({ text: "boom again" })} />));
    expect(text()).not.toContain("tool failed");
  });

  it.each([
    ["memory recall", fixtures.memoryRecallList],
    ["wakeup", fixtures.scheduleWakeup],
    ["memory file", makeToolCall({ kind: "read", args_preview: args({ file_path: `${MEMORY_DIR}/feedback.md` }) })],
  ])("auto-opens and folds a failed %s card", (_label, tool) => {
    const { text, toggle } = renderCard(tool, makeError({ text: "kind-specific boom" }), { toolKey: "claude" });
    expect(text()).toContain("tool failed");
    toggle();
    expect(text()).not.toContain("tool failed");
  });
});

describe("structured output media", () => {
  const withOutput = (output: ToolOutputBlock[]) => renderCard(fixtures.generic, makeCompletion({ output }));

  it.each<[string, ToolOutputBlock, string, string | null, string]>([
    ["image data", { kind: "image", mime_type: "image/png", data: "AAAA" }, "img", "data:image/png;base64,AAAA", ""],
    ["audio data", { kind: "audio", mime_type: "audio/wav", data: "QUJD" }, "audio", "data:audio/wav;base64,QUJD", ""],
    [
      "link",
      { kind: "resource_link", uri: "file:///report.pdf", name: "report.pdf" },
      "a",
      "file:///report.pdf",
      "report.pdf",
    ],
    [
      "blob download",
      { kind: "resource", uri: "file:///out.bin", mime_type: "application/octet-stream", data: "QkxPQg==" },
      'a[download="out.bin"]',
      "data:application/octet-stream;base64,QkxPQg==",
      "out.bin",
    ],
    ["text", { kind: "text", text: "structured text" }, "pre", null, "structured text"],
    ["text resource", { kind: "resource", uri: "file:///x.txt", text: "resource body" }, "pre", null, "resource body"],
  ])("renders %s", (_label, block, selector, attr, visible) => {
    const { container, text } = withOutput([block]);
    const el = [...container.querySelectorAll(selector)].at(-1);
    expect(el).toBeTruthy();
    if (attr) expect(el!.getAttribute(selector === "a" || selector.startsWith("a[") ? "href" : "src")).toBe(attr);
    expect(text()).toContain(visible);
  });

  // Agent-controlled uris never reach a sink; unusable blocks degrade to a placeholder.
  it.each<[string, ToolOutputBlock, string, string]>([
    ["image without data", { kind: "image", mime_type: "image/png" }, "img", "image (image/png)"],
    [
      "javascript image",
      { kind: "image", mime_type: "image/png", uri: "javascript:alert(1)" },
      "img",
      "image (image/png)",
    ],
    ["audio without data", { kind: "audio", mime_type: "audio/wav" }, "audio", "audio (audio/wav)"],
    ["javascript link", { kind: "resource_link", uri: "javascript:alert(1)", name: "evil.html" }, "a", "evil.html"],
  ])("degrades %s to a placeholder", (_label, block, selector, label) => {
    const { container, text } = withOutput([block]);
    expect(container.querySelector(selector)).toBeNull();
    expect(text()).toContain(label);
  });
});

describe("repo-relative paths", () => {
  const session: FileRefSession = { project_path: "/tmp", main_repo_path: null, workspace_repos: [] };
  const multi: FileRefSession = {
    project_path: "/tmp/ws",
    main_repo_path: null,
    workspace_repos: [{ name: "api", source_path: "/tmp/api" }],
  };
  const editAt = (file_path: string) =>
    makeToolCall({ kind: "edit", args_preview: args({ file_path, old_string: "a", new_string: "b" }) });

  it.each<[string, ToolCall, FileRefSession, string, string | null]>([
    ["edit", fixtures.edit, session, "main.rs", "/tmp/main.rs"],
    ["read", fixtures.read, session, "main.rs", "/tmp/main.rs"],
    ["delete", fixtures.del, session, "gone.rs", "/tmp/gone.rs"],
    ["write", fixtures.write, session, "new.rs", "/tmp/new.rs"],
    ["multi-repo workspace", editAt("/tmp/api/src/h.ts"), multi, "api/src/h.ts", "/tmp/api/src/h.ts"],
    ["outside every root", editAt("/etc/hosts"), session, "/etc/hosts", null],
  ])("%s", (_label, tool, fileRefSession, shown, hidden) => {
    const { text } = renderCard(tool, undefined, { session: fileRefSession });
    expect(text()).toContain(shown);
    if (hidden) expect(text()).not.toContain(hidden);
  });

  it("renders each multi-file diff header relative", () => {
    const tool = makeToolCall({
      kind: "edit",
      diffs: [
        { path: "/tmp/src/alpha.rs", old_text: "a", new_text: "b", created_at: "2026-05-21T00:00:00Z" },
        { path: "/tmp/src/beta.rs", old_text: null, new_text: "c", created_at: "2026-05-21T00:00:00Z" },
      ],
    });
    const { text, toggle } = renderCard(tool, undefined, { session });
    expect(text()).toContain("src/alpha.rs");
    expect(text()).toContain("+1 more");
    expect(text()).not.toContain("/tmp/src/alpha.rs");
    toggle();
    expect(text()).toContain("src/beta.rs");
    expect(text()).not.toContain("/tmp/src/beta.rs");
  });

  it.each([
    ["read", "read"],
    ["edit", "write"],
    ["delete", "delete"],
  ])("falls back to (unknown file) for a path-less %s tool", (kind, label) => {
    const { text } = renderCard(makeToolCall({ name: "", kind }), undefined, { session });
    expect(text()).toContain(label);
    expect(text()).toContain("(unknown file)");
  });

  it("keeps the absolute path in the title tooltip", () => {
    const { container } = renderCard(fixtures.edit, undefined, { session });
    expect(container.querySelector('[title="/tmp/main.rs"]')!.textContent).toContain("main.rs");
  });

  it.each([
    [fixtures.read, "/tmp/main.rs"],
    [fixtures.write, "/tmp/new.rs"],
  ])("opens %#'s file via onOpenFileRef, or renders plain text without a handler", (tool, path) => {
    const onOpenFileRef = vi.fn();
    const { container } = renderCard(tool, undefined, { session, onOpenFileRef });
    fireEvent.click(container.querySelector(`button[title="${path}"]`)!);
    expect(onOpenFileRef).toHaveBeenCalledWith({ path });
    cleanup();
    const plain = renderCard(tool, undefined, { session });
    expect(plain.container.querySelector(`button[title="${path}"]`)).toBeNull();
    expect(plain.container.querySelector(`[title="${path}"]`)).not.toBeNull();
  });
});

describe("TodoGroupCard", () => {
  const snapshot = (
    id: string,
    content: string,
    status = "in_progress",
    result = makeCompletion({ toolCallId: id }),
  ) => ({
    tool: makeToolCall({ id, name: "TodoWrite", args_preview: args({ todos: [{ content, status }] }) }),
    result,
  });
  const items = [snapshot("td1", "Step Alpha"), snapshot("td2", "Step Bravo"), snapshot("td3", "Step Charlie")];
  const renderGroup = (groupItems: typeof items, toolKey = "claude") =>
    render(wrap(<TodoGroupCard items={groupItems} />, { toolKey }));

  it("shows only the latest snapshot collapsed and every snapshot in order when expanded", () => {
    const { container, getByRole } = renderGroup(items);
    expect(container.textContent).toContain("updated 3 times");
    expect(container.textContent).toContain("Step Charlie");
    expect(container.textContent).toContain("1 active");
    expect(container.textContent).not.toContain("Step Alpha");
    fireEvent.click(getByRole("button"));
    const text = container.textContent ?? "";
    expect(text.indexOf("Step Alpha")).toBeGreaterThan(-1);
    expect(text.indexOf("Step Alpha")).toBeLessThan(text.indexOf("Step Bravo"));
    expect(text).toContain("1 items");
  });

  it.each([
    // An empty clear counts toward the fold and previews as cleared.
    [
      "empty clear",
      {
        tool: makeToolCall({ id: "c", name: "TodoWrite", kind: "think", args_preview: args({ todos: [] }) }),
        result: makeCompletion(),
      },
      ["updated 4 times", "todos cleared"],
      ["Step Charlie"],
    ],
    // Failed and stopped tails preview the last live snapshot but label the header.
    [
      "failed tail",
      snapshot("td4", "Broken plan", "in_progress", makeError()),
      ["Step Charlie", "failed"],
      ["Broken plan"],
    ],
    [
      "stopped tail",
      snapshot("td4", "Interrupted plan", "in_progress", makeStopped()),
      ["Step Charlie", "stopped"],
      ["Interrupted plan", "done"],
    ],
  ])("%s", (_label, tail, contains, excludes) => {
    const { container } = renderGroup([...items, tail]);
    for (const s of contains) expect(container.textContent).toContain(s);
    for (const s of excludes) expect(container.textContent).not.toContain(s);
  });

  it("keeps opencode todowrite groups visible", () => {
    const oc = ["OpenCode Alpha", "OpenCode Bravo", "OpenCode Charlie"].map((c, i) => ({
      ...snapshot(`oc${i}`, c),
      tool: makeToolCall({
        id: `oc${i}`,
        name: `${i} todos`,
        args_preview: args({ todos: [{ content: c, status: "pending" }] }),
      }),
    }));
    const { container } = renderGroup(oc, "opencode");
    expect(container.textContent).toContain("updated 3 times");
    expect(container.textContent).toContain("OpenCode Charlie");
    expect(container.textContent).not.toContain("OpenCode Alpha");
  });

  it("renders nothing when no item is a todo write", () => {
    const { container } = renderGroup([{ tool: fixtures.bash, result: makeCompletion() }]);
    expect(container.textContent).toBe("");
  });
});

describe("ToolGroupCard", () => {
  const item = (
    id: string,
    kind: string,
    argsPreview: Record<string, unknown>,
    result = makeCompletion({ toolCallId: id }),
  ) => ({
    tool: makeToolCall({ id, kind, name: kind, args_preview: args(argsPreview) }),
    result,
    kind,
  });

  it("tallies kinds and errors, and renders children when expanded", () => {
    const items = [
      item("g1", "execute", { command: "ls" }),
      item("g2", "read", { path: "a.ts" }),
      item("g3", "read", { path: "b.ts" }, makeError()),
      item("d1", "delete", { path: "z.ts" }),
      item("f1", "fetch", { url: "https://y" }),
      item("t1", "think", {}),
      item("o1", "switch_mode", {}),
    ];
    const { container, getAllByRole } = render(<ToolGroupCard items={items} />);
    for (const s of ["7 actions", "Read 2", "Bash 1", "Delete 1", "Fetch 1", "Think 1", "Switch_mode 1", "1 error"]) {
      expect(container.textContent).toContain(s);
    }
    fireEvent.click(getAllByRole("button")[0]!);
    expect(container.textContent).toContain("z.ts");
  });

  it("renders nothing for an empty run", () => {
    expect(render(<ToolGroupCard items={[]} />).container.textContent).toBe("");
  });
});

describe("SubagentCard", () => {
  it("shows the task, child count, and children when expanded", () => {
    const tool = makeToolCall({ id: "task-1", name: "Task", args_preview: args({ description: "investigate bug" }) });
    const children = [
      {
        tool: makeToolCall({ id: "c1", kind: "read", args_preview: args({ path: "x.ts" }) }),
        result: makeCompletion(),
      },
    ];
    const { container, getByRole } = render(<SubagentCard tool={tool} result={makeCompletion()} children={children} />);
    expect(container.textContent).toContain("subagent");
    expect(container.textContent).toContain("investigate bug");
    expect(container.textContent).toContain("1 tool");
    fireEvent.click(getByRole("button"));
    expect(container.textContent).toContain("x.ts");
  });

  it("renders an off-protocol launch with its prompt and unwrapped report", () => {
    const tool = makeToolCall({
      kind: "think",
      args_preview: args({ description: "Trace clear session resets", prompt: "Research only, do not edit files." }),
    });
    const result = makeCompletion({
      text: '<task id="ses_1" state="completed"><task_result>No files edited. Conclusion: safe.</task_result></task>',
    });
    const { container, getByRole } = render(<SubagentCard tool={tool} result={result} children={[]} />);
    expect(container.textContent).toContain("Trace clear session resets");
    expect(container.textContent).not.toContain("0 tools");
    fireEvent.click(getByRole("button"));
    expect(container.textContent).toContain("Research only, do not edit files.");
    expect(container.textContent).toContain("No files edited. Conclusion: safe.");
    expect(container.textContent).not.toContain("task_result");
  });

  it.each([
    ['<task id="ses_1" state="completed"><task_result>done here</task_result></task>', "done here"],
    ['\n  <task id="s"><task_result>\n  ok\n  </task_result></task>  \n', "ok"],
    ["just a plain report", "just a plain report"],
    ["<task_result>no closing task tag", "<task_result>no closing task tag"],
    ["<task><task_result>use Vec<String> and a < b</task_result></task>", "use Vec<String> and a < b"],
  ])("extractTaskResult(%j)", (input, expected) => {
    expect(extractTaskResult(input)).toBe(expected);
  });
});

describe("AsyncSubagentCard", () => {
  const tool = makeToolCall({
    id: "task-async",
    kind: "think",
    args_preview: args({ description: "Map backend lifecycle" }),
  });

  it("is a neutral background card before any tailer event, never leaking the launch marker", () => {
    const { container } = render(<AsyncSubagentCard tool={tool} />);
    expect(container.textContent).toContain("Map backend lifecycle");
    expect(container.textContent).toContain("runs in background");
    expect(container.textContent).not.toMatch(/agentId|Async agent launched|internal ID/);
  });

  it("reflects the live background-agent record", () => {
    const agent: BackgroundAgent = {
      agentId: "a1",
      toolCallId: "task-async",
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
    expect(container.textContent).toContain("3 tools · Read");
    expect(container.textContent).not.toContain("agentId");
  });

  it("opens the Sub agents pane on click", () => {
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

describe("duration formatting", () => {
  it.each([
    [0, "0 ms"],
    [999, "999 ms"],
    [1000, "1.0s"],
    [4231, "4.2s"],
    [59_999, "60.0s"],
    [60_000, "1m 0s"],
    [90_000, "1m 30s"],
    [3_661_000, "61m 1s"],
  ])("formatDurationMs(%i) = %s", (ms, expected) => {
    expect(formatDurationMs(ms)).toBe(expected);
  });

  it.each([
    [45, "45s"],
    [180, "3m"],
    [194, "3m 14s"],
    [3600, "1h"],
    [4020, "1h 7m"],
    [2 * 86400, "2d"],
    [2 * 86400 + 4 * 3600, "2d 4h"],
  ])("formatDurationSeconds(%i) = %s", (s, expected) => {
    expect(formatDurationSeconds(s)).toBe(expected);
  });
});

describe("HighlightedBlock stale-content transitions (#3974)", () => {
  afterEach(() => {
    vi.mocked(highlightSnippet).mockReset();
    vi.mocked(highlightSnippet).mockResolvedValue(null);
  });

  const readCard = (path: string, body: string) =>
    wrap(
      <ToolCard
        tool={makeToolCall({ kind: "read", args_preview: args({ file_path: path }) })}
        result={makeCompletion({ text: body })}
      />,
    );

  it("clears highlighted output when a reused read card's file becomes extensionless", async () => {
    vi.mocked(highlightSnippet).mockResolvedValueOnce('<pre class="shiki">highlighted rust</pre>');

    const { container, rerender } = render(readCard("/tmp/a.rs", "fn main() {}"));
    fireEvent.click(container.querySelector("button")!);

    await waitFor(() => {
      expect(container.querySelector("pre.shiki")).toBeTruthy();
    });

    vi.mocked(highlightSnippet).mockResolvedValueOnce(null);
    rerender(readCard("/tmp/README", "plain readme text"));

    expect(container.querySelector("pre.shiki")).toBeNull();
    expect(container.textContent).toContain("plain readme text");
    expect(container.textContent).not.toContain("fn main");
  });

  it("clears highlighted output when a reused read card's highlight rejects", async () => {
    vi.mocked(highlightSnippet).mockResolvedValueOnce('<pre class="shiki">highlighted rust</pre>');

    const { container, rerender } = render(readCard("/tmp/a.rs", "fn main() {}"));
    fireEvent.click(container.querySelector("button")!);

    await waitFor(() => {
      expect(container.querySelector("pre.shiki")).toBeTruthy();
    });

    vi.mocked(highlightSnippet).mockRejectedValueOnce(new Error("boom"));
    rerender(readCard("/tmp/b.rs", "fn other() {}"));

    await waitFor(() => {
      expect(container.textContent).toContain("fn other() {}");
    });
    expect(container.querySelector("pre.shiki")).toBeNull();
    expect(container.textContent).not.toContain("fn main");
  });

  it("ignores a late resolution from a superseded request (pending A \u2192 committed B \u2192 late A)", async () => {
    let resolveA!: (v: string | null) => void;
    vi.mocked(highlightSnippet).mockReturnValueOnce(
      new Promise<string | null>((res) => {
        resolveA = res;
      }),
    );

    const { html, text } = await renderWithLateResolution({
      a: readCard("/tmp/a.rs", "fn a() {}"),
      b: readCard("/tmp/README", "plain readme text"),
      bText: "plain readme text",
      resolveStale: () => resolveA('<pre class="shiki">OLD_A</pre>'),
      // Expand the card so the highlighted body renders.
      afterMount: (host) => host.querySelector("button")!.dispatchEvent(new MouseEvent("click", { bubbles: true })),
    });

    expect(text).toContain("plain readme text");
    expect(html).not.toContain("OLD_A");
  });
});

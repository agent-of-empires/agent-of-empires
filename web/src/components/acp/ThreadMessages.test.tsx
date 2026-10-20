// @vitest-environment jsdom
//
// A bare <img src="/api/.../attachments/..."> can't carry the passphrase-mode
// device-binding header, so it renders broken. Pin that the Image part
// resolves through ArtifactImage's authenticated fetch instead (see artifactMedia.tsx).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import {
  AssistantRuntimeProvider,
  ThreadPrimitive,
  useExternalStoreRuntime,
  type ThreadMessageLike,
} from "@assistant-ui/react";

import { parseJsonObject } from "../../lib/acpArgs";
import { writeClipboard } from "../../lib/clipboard";
import * as openInNewTabModule from "../../lib/openInNewTab";
import type { ActivityRow, ToolCall } from "../../lib/acpTypes";
import { activityToThreadMessages, SUBAGENT_TASK_NAME, TODO_GROUP_NAME, TOOL_GROUP_NAME } from "./activityMessages";
import { AssistantMessage, UserMessage } from "./ThreadMessages";

vi.mock("../../lib/clipboard", () => ({ writeClipboard: vi.fn().mockResolvedValue(true) }));

vi.mock("../../lib/acpArgs", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/acpArgs")>();
  return { ...actual, parseJsonObject: vi.fn(actual.parseJsonObject) };
});

const ATTACHMENT_URL = "/api/sessions/s1/acp/attachments/att1";

function Harness({ messages }: { messages: ThreadMessageLike[] }) {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <ThreadPrimitive.Messages components={{ UserMessage, AssistantMessage }} />
    </AssistantRuntimeProvider>
  );
}

beforeEach(() => {
  vi.stubGlobal("URL", {
    createObjectURL: vi.fn(() => "blob:mock-url"),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("UserMessage image part", () => {
  it("renders an attachment through authenticated fetch, never as a bare <img src>", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["x"]) }));
    const { container } = render(
      <Harness messages={[{ role: "user", content: [{ type: "image", image: ATTACHMENT_URL }] }]} />,
    );

    await waitFor(() => {
      const img = container.querySelector("img.acp-artifact-image");
      expect(img).not.toBeNull();
      expect(img?.getAttribute("src")).toBe("blob:mock-url");
    });
    expect(fetch).toHaveBeenCalledWith(ATTACHMENT_URL);
    expect(container.querySelector(`img[src="${ATTACHMENT_URL}"]`)).toBeNull();
  });
});

describe("UserMessage attachment bubble", () => {
  it("renders the image and the text in one bubble, image first", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["x"]) }));
    const rows: ActivityRow[] = [
      {
        id: "p1",
        kind: "user_prompt",
        text: "look at this",
        at: "2026-10-06T00:00:00Z",
        attachments: [{ id: "att1", kind: "image", mimeType: "image/png", url: ATTACHMENT_URL }],
      },
    ];
    const { container } = render(<Harness messages={activityToThreadMessages(rows, false)} />);

    const bubbles = container.querySelectorAll('[data-testid="user-message-bubble"]');
    expect(bubbles).toHaveLength(1);
    await waitFor(() => expect(bubbles[0]!.querySelector("img.acp-artifact-image")).not.toBeNull());
    const img = bubbles[0]!.querySelector("img.acp-artifact-image")!;
    const text = Array.from(bubbles[0]!.querySelectorAll("p")).find((p) => p.textContent === "look at this")!;
    expect(img.compareDocumentPosition(text) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });
});

describe("UserMessage image click", () => {
  it("expands the attachment in an overlay and closes on Escape or backdrop click", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["x"]) }));
    const { container } = render(
      <Harness messages={[{ role: "user", content: [{ type: "image", image: ATTACHMENT_URL }] }]} />,
    );
    await waitFor(() => expect(container.querySelector("img.acp-artifact-image")).not.toBeNull());
    expect(screen.queryByTestId("image-lightbox")).toBeNull();

    const thumb = container.querySelector("img.acp-artifact-image")!.closest("button")!;
    fireEvent.click(thumb);
    expect(screen.getByTestId("image-lightbox").querySelector("img")?.getAttribute("src")).toBe("blob:mock-url");
    // Focus moves to the close button and Tab cannot reach the thread behind it.
    const close = screen.getByRole("button", { name: "Close image" });
    expect(document.activeElement).toBe(close);
    expect(fireEvent.keyDown(document, { key: "Tab" })).toBe(false);

    // The app dispatches global shortcuts from a document capture listener; it must not see keys while the modal is open.
    const globalShortcut = vi.fn();
    document.addEventListener("keydown", globalShortcut, true);
    fireEvent.keyDown(document, { key: "`", code: "Backquote", ctrlKey: true });
    expect(globalShortcut).not.toHaveBeenCalled();

    fireEvent.keyDown(document, { key: "Escape" });
    document.removeEventListener("keydown", globalShortcut, true);
    expect(screen.queryByTestId("image-lightbox")).toBeNull();
    expect(document.activeElement).toBe(thumb);

    fireEvent.click(thumb);
    fireEvent.click(screen.getByRole("button", { name: "Close image" }));
    expect(screen.queryByTestId("image-lightbox")).toBeNull();
    expect(document.activeElement).toBe(thumb);

    fireEvent.click(container.querySelector("img.acp-artifact-image")!);
    fireEvent.click(screen.getByTestId("image-lightbox"));
    expect(screen.queryByTestId("image-lightbox")).toBeNull();
  });
});

describe("UserMessage file attachment", () => {
  it("shares the bubble with the text and opens through the authenticated opener", () => {
    const open = vi.spyOn(openInNewTabModule, "openInNewTab").mockResolvedValue({ ok: true });
    const rows: ActivityRow[] = [
      {
        id: "p2",
        kind: "user_prompt",
        text: "see doc",
        at: "2026-10-06T00:00:00Z",
        attachments: [
          { id: "att2", kind: "resource", mimeType: "application/pdf", name: "spec.pdf", size: 1, url: "/api/f/att2" },
        ],
      },
    ];
    const { container } = render(<Harness messages={activityToThreadMessages(rows, false)} />);
    const bubble = container.querySelector('[data-testid="user-message-bubble"]')!;
    expect(bubble.textContent).toContain("spec.pdf");
    expect(bubble.textContent).toContain("see doc");

    fireEvent.click(screen.getByText("spec.pdf"));
    expect(open).toHaveBeenCalledWith("/api/f/att2", "spec.pdf");
  });
});

describe("UserMessage plugin origin", () => {
  const at = "2026-10-06T00:00:00Z";
  const prompt = (id: string, originPlugin?: string): ActivityRow => ({
    id,
    kind: "user_prompt",
    text: `text ${id}`,
    at,
    ...(originPlugin ? { originPlugin } : {}),
  });

  it("labels a plugin-sent prompt with its sender and leaves a typed one unlabelled", () => {
    const { container } = render(
      <Harness messages={activityToThreadMessages([prompt("typed"), prompt("sent", "cron")], false)} />,
    );
    const labels = container.querySelectorAll('[data-testid="user-message-origin"]');
    expect(labels).toHaveLength(1);
    expect(labels[0]!.textContent).toBe("Sent by plugin cron");
    expect(container.textContent).toContain("text sent");
    expect(container.textContent).toContain("text typed");
  });
});

describe("AssistantMessage compaction summary", () => {
  it("renders the retained summary collapsed under its own label", () => {
    const rows: ActivityRow[] = [
      {
        id: "compaction-summary-1",
        kind: "compaction_summary",
        text: "Kept **PINEAPPLE-42**",
        at: "2026-10-06T00:00:00Z",
      },
      // An empty row only anchors a summary that has not arrived yet.
      { id: "compaction-summary-2", kind: "compaction_summary", text: "", at: "2026-10-06T00:00:00Z" },
    ];
    const { container } = render(<Harness messages={activityToThreadMessages(rows, false)} />);
    expect(container.querySelectorAll("details")).toHaveLength(1);
    const details = container.querySelector("details");
    expect(details).not.toBeNull();
    expect(details!.open).toBe(false);
    expect(details!.querySelector("summary")?.textContent).toContain("Compaction summary");
    expect(details!.textContent).toContain("PINEAPPLE-42");
  });
});

describe("AssistantMessage table copy across an interrupted turn", () => {
  const at = "2026-05-12T00:00:00Z";
  const CUT_OFF = "| a | b |\n|---|---|\n| 1 | 2";
  const thread = (busy: boolean) =>
    activityToThreadMessages(
      [
        { id: "u1", kind: "user_prompt", text: "go", at },
        { id: "m1", kind: "message", text: CUT_OFF, at },
      ],
      busy,
    );

  it("offers no copy while the turn runs, then copies the received rows once it stops mid-row", async () => {
    const { container, rerender, getByRole, queryByRole } = render(<Harness messages={thread(true)} />);
    // The display has caught up with everything received, so only the running turn withholds the copy.
    await waitFor(() => expect(container.querySelector("tbody td:last-child")?.textContent).toBe("2"));
    expect(queryByRole("button", { name: /copy table/i })).toBeNull();

    rerender(<Harness messages={thread(false)} />);
    fireEvent.click(getByRole("button", { name: "Copy table as markdown" }));
    await waitFor(() => expect(writeClipboard).toHaveBeenCalledWith(CUT_OFF));
  });
});

describe("AssistantMessage group parts", () => {
  const at = "2026-05-12T00:00:00Z";
  const start = (id: string, tool: Partial<ToolCall> = {}): ActivityRow => ({
    id: `start-${id}`,
    kind: "tool_start",
    text: "Read",
    at,
    toolCallId: id,
    tool: { id, name: "Read", kind: "read", args_preview: "{}", started_at: at, ...tool },
  });
  const todo = (id: string) =>
    start(id, { name: "TodoWrite", kind: "think", args_preview: JSON.stringify({ todos: [] }) });
  const task = start("task-1", { name: "Task", kind: "think", args_preview: '{"description":"go"}' });
  const child = (id: string) => start(id, { parent_tool_call_id: "task-1" });

  it.each<[string, string, ActivityRow[]]>([
    ["tool group", TOOL_GROUP_NAME, [start("t1"), start("t2"), start("t3")]],
    ["todo group", TODO_GROUP_NAME, [todo("td1"), todo("td2"), todo("td3")]],
    ["subagent", SUBAGENT_TASK_NAME, [task, child("c1"), child("c2")]],
    [
      "async subagent",
      SUBAGENT_TASK_NAME,
      [
        task,
        { id: "done-task-1", kind: "tool_complete", text: "launched", at, toolCallId: "task-1", asyncSubagent: true },
      ],
    ],
  ])("does not re-parse an unchanged %s payload when the message re-renders", (_label, toolName, tools) => {
    const messages = (reply: string) =>
      activityToThreadMessages(
        [{ id: "u1", kind: "user_prompt", text: "go", at }, ...tools, { id: "m1", kind: "message", text: reply, at }],
        false,
      );
    const groupArgs = messages("a")
      .flatMap((m) => m.content as { toolName?: string; argsText?: string }[])
      .find((p) => p.toolName === toolName)?.argsText;
    expect(groupArgs).toBeTruthy();
    const groupParses = () => vi.mocked(parseJsonObject).mock.calls.filter(([s]) => s === groupArgs).length;

    const { rerender, getByText } = render(<Harness messages={messages("a")} />);
    const afterMount = groupParses();
    expect(afterMount).toBeGreaterThan(0);
    // A streaming reply grows the text part while the group's payload stays the same.
    rerender(<Harness messages={messages("a longer reply")} />);
    getByText("a longer reply");
    expect(groupParses()).toBe(afterMount);
  });
});

describe("AssistantMessage tool output media", () => {
  const at = "2026-05-12T00:00:00Z";
  // The file-level URL stub has no constructor, which link-scheme checks need.
  beforeEach(() => vi.unstubAllGlobals());
  const rows = (output: NonNullable<ActivityRow["output"]>, kind = "other"): ActivityRow[] => [
    { id: "u1", kind: "user_prompt", text: "go", at },
    {
      id: "start-t1",
      kind: "tool_start",
      text: "Capture",
      at,
      toolCallId: "t1",
      tool: { id: "t1", name: "Capture", kind, args_preview: '{"path":"a.rs"}', started_at: at },
    },
    { id: "done-t1", kind: "tool_complete", text: "Captured", at, toolCallId: "t1", output },
  ];

  it("renders a tool result's links and embedded text under its card", () => {
    const { getByText, getByRole } = render(
      <Harness
        messages={activityToThreadMessages(
          rows([
            { kind: "resource_link", uri: "https://example.com/report", name: "Full report" },
            { kind: "resource", uri: "file:///notes.md", mime_type: "text/markdown", text: "first finding" },
          ]),
          false,
        )}
      />,
    );
    expect(getByRole("link", { name: "Full report" }).getAttribute("href")).toBe("https://example.com/report");
    getByText("first finding");
  });

  it.each([
    ["with child cards", true],
    ["without any", false],
  ])("renders a subagent parent's own media under its card %s", (_name, withChild) => {
    const tool = (id: string, extra: Partial<ToolCall> = {}) => ({
      id,
      name: "Task",
      kind: "think",
      args_preview: '{"description":"go"}',
      started_at: at,
      ...extra,
    });
    const link = { kind: "resource_link" as const, uri: "https://example.com/report", name: "Full report" };
    const subagentRows: ActivityRow[] = [
      { id: "u1", kind: "user_prompt", text: "go", at },
      { id: "start-task-1", kind: "tool_start", text: "Task", at, toolCallId: "task-1", tool: tool("task-1") },
      ...(withChild
        ? ([
            {
              id: "start-c1",
              kind: "tool_start",
              text: "Read",
              at,
              toolCallId: "c1",
              tool: tool("c1", { name: "Read", kind: "read", parent_tool_call_id: "task-1" }),
            },
            { id: "done-c1", kind: "tool_complete", text: "ok", at, toolCallId: "c1" },
          ] as ActivityRow[])
        : []),
      { id: "done-task-1", kind: "tool_complete", text: "done", at, toolCallId: "task-1", output: [link] },
    ];
    const { getByRole } = render(<Harness messages={activityToThreadMessages(subagentRows, false)} />);
    expect(getByRole("link", { name: "Full report" }).getAttribute("href")).toBe("https://example.com/report");
  });

  it("renders an inline image result", () => {
    const { getByAltText } = render(
      <Harness
        messages={activityToThreadMessages(rows([{ kind: "image", mime_type: "image/png", data: "AAAA" }]), false)}
      />,
    );
    expect(getByAltText("tool output image (image/png)").getAttribute("src")).toBe("data:image/png;base64,AAAA");
  });

  it("keeps a Read's image result inside its card, collapsed until opened", () => {
    const { queryByAltText, getByAltText, getByRole } = render(
      <Harness
        messages={activityToThreadMessages(
          rows([{ kind: "image", mime_type: "image/png", data: "AAAA" }], "read"),
          false,
        )}
      />,
    );
    expect(queryByAltText("tool output image (image/png)")).toBeNull();
    fireEvent.click(getByRole("button", { name: /read/i }));
    expect(getByAltText("tool output image (image/png)").closest("[data-tool-id]")).not.toBeNull();
  });

  it("expands an inline image result in the lightbox", () => {
    const { getByAltText } = render(
      <Harness
        messages={activityToThreadMessages(rows([{ kind: "image", mime_type: "image/png", data: "AAAA" }]), false)}
      />,
    );
    fireEvent.click(getByAltText("tool output image (image/png)"));
    expect(screen.getByTestId("image-lightbox").querySelector("img")?.getAttribute("src")).toBe(
      "data:image/png;base64,AAAA",
    );
  });
});

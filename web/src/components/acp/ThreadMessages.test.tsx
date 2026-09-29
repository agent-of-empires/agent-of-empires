// @vitest-environment jsdom
//
// A bare <img src="/api/.../attachments/..."> can't carry the passphrase-mode
// device-binding header, so it renders broken. Pin that the Image part
// resolves through ArtifactImage's authenticated fetch instead (see artifactMedia.tsx).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";
import {
  AssistantRuntimeProvider,
  ThreadPrimitive,
  useExternalStoreRuntime,
  type ThreadMessageLike,
} from "@assistant-ui/react";

import { parseJsonObject } from "../../lib/acpArgs";
import type { ActivityRow, ToolCall } from "../../lib/acpTypes";
import { activityToThreadMessages, SUBAGENT_TASK_NAME, TODO_GROUP_NAME, TOOL_GROUP_NAME } from "./activityMessages";
import { AssistantMessage, UserMessage } from "./ThreadMessages";

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

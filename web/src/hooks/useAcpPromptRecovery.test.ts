// @vitest-environment jsdom

import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { emptyAcpState } from "../lib/acpTypes";
import { clearAcpCache, useAcpSession } from "./useAcpSession";
import { flushAsync, installAcpFakes, json, lastSocket, profileWrapper } from "./__tests__/acpHarness";

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  clearAcpCache();
});
beforeEach(() => {
  localStorage.clear();
  clearAcpCache();
});

const old = { id: "old-reply", kind: "message", text: "Earlier reply", at: "2026-01-01T00:00:00Z" };
const reply = { id: "msg-102", kind: "message", text: "Already working", at: "2026-01-01T00:00:02Z" };
const attachment = { id: "server-image", kind: "image", mime_type: "image/png", name: "image.png", size: 3 };

async function open() {
  const hook = renderHook(() => useAcpSession("prompt-recovery"), { wrapper: profileWrapper });
  await flushAsync();
  lastSocket().open();
  await flushAsync();
  return hook;
}

describe("sent prompt transcript recovery", () => {
  it.each([false, true])("does not recover normal delivery (canonical before ACK: %s)", async (rowFirst) => {
    let id = "";
    const calls = installAcpFakes(({ url, body }) => {
      if (!url.includes("/acp/prompt")) return undefined;
      id = JSON.parse(body!).prompt_id;
      return json({ disposition: "sent" }, 202);
    });
    const { result } = await open();
    const count = calls.filter((call) => call.url.includes("/acp/replay")).length;
    await act(() => result.current.sendPrompt("normal"));
    const ack = () =>
      lastSocket().message({
        session_id: "prompt-recovery",
        seq: 1,
        event: { UserPromptSent: { prompt_id: id, text: "normal" } },
      });
    const row = () =>
      lastSocket().message({
        kind: "transcript_delta",
        delta: { Append: { id, kind: "user_prompt", text: "normal", at: "t" } },
      });
    if (rowFirst) {
      row();
      ack();
    } else {
      ack();
      row();
    }
    lastSocket().message({ session_id: "prompt-recovery", seq: 2, event: { AgentMessageChunk: { text: "reply" } } });
    lastSocket().message({ kind: "transcript_delta", delta: { Append: reply } });
    await flushAsync();
    expect(result.current.state.optimisticRows).toEqual([]);
    expect(calls.filter((call) => call.url.includes("/acp/replay"))).toHaveLength(count);
  });

  it.each(["synthesized", "foreign", "unrelated", "missing"])(
    "does not treat a %s ACK as proof for this prompt",
    async (kind) => {
      let id = "";
      const calls = installAcpFakes(({ url, body }) => {
        if (!url.includes("/acp/prompt")) return undefined;
        id = JSON.parse(body!).prompt_id;
        return json({ disposition: "sent" }, 202);
      });
      const { result } = await open();
      const count = calls.filter((call) => call.url.includes("/acp/replay")).length;
      await act(() => result.current.sendPrompt("keep my text"));
      let session = "prompt-recovery";
      let echoed: string | null = id;
      if (kind === "foreign") session = "another-session";
      if (kind === "unrelated") echoed = "not-this-send";
      if (kind === "missing") echoed = null;
      lastSocket().message({
        session_id: session,
        seq: 1,
        event: { UserPromptSent: { prompt_id: echoed, text: "keep my text", synthesized: kind === "synthesized" } },
      });
      lastSocket().message({ session_id: "prompt-recovery", seq: 2, event: { AgentMessageChunk: { text: "reply" } } });
      lastSocket().message({ kind: "transcript_delta", delta: { Append: reply } });
      await flushAsync();
      expect(calls.filter((call) => call.url.includes("/acp/replay"))).toHaveLength(count);
      expect(result.current.state.optimisticRows).toMatchObject([{ id, text: "keep my text" }]);
    },
  );

  it.each([false, true])("a foreign ACK never settles this send (late ACK: %s)", async (lateAck) => {
    let id = "";
    const calls = installAcpFakes(({ url, body }) => {
      if (!url.includes("/acp/prompt")) return undefined;
      id = JSON.parse(body!).prompt_id;
      return json({ disposition: "sent" }, 202);
    });
    const { result } = await open();
    const count = calls.filter((call) => call.url.includes("/acp/replay")).length;
    await act(() => result.current.sendPrompt("keep my send pending"));
    if (lateAck)
      lastSocket().message({ session_id: "prompt-recovery", seq: 10, event: { Stopped: { reason: "done" } } });
    lastSocket().message({
      session_id: "another-session",
      seq: 1,
      event: { UserPromptSent: { prompt_id: id, text: "keep my send pending" } },
    });
    await flushAsync();
    expect(result.current.state.inflightPromptIds).toEqual([id]);
    expect(result.current.state.optimisticRows).toMatchObject([{ id }]);
    expect(calls.filter((call) => call.url.includes("/acp/replay"))).toHaveLength(count);
  });

  it("recovers when ACK and reply arrive before the optimistic-row commit", async () => {
    let id = "";
    let reads = 0;
    installAcpFakes(({ url, body }) => {
      if (url.includes("/acp/prompt")) {
        id = JSON.parse(body!).prompt_id;
        const socket = lastSocket();
        socket.onmessage?.({
          data: JSON.stringify({
            session_id: "prompt-recovery",
            seq: 101,
            event: { UserPromptSent: { prompt_id: id, text: "batched send" } },
          }),
        } as MessageEvent);
        socket.onmessage?.({
          data: JSON.stringify({
            session_id: "prompt-recovery",
            seq: 102,
            event: { AgentMessageChunk: { text: reply.text } },
          }),
        } as MessageEvent);
        socket.onmessage?.({
          data: JSON.stringify({ kind: "transcript_delta", delta: { Append: reply } }),
        } as MessageEvent);
        return json({ disposition: "sent" }, 202);
      }
      if (!url.includes("/acp/replay")) return undefined;
      const params = new URL(url, "http://x").searchParams;
      if (id && params.get("since") === "100" && params.get("view") === "rows") {
        reads += 1;
        return json({
          frames: [],
          rows: [{ id, kind: "user_prompt", text: "batched send", at: "t" }, reply],
          highest_seq: 102,
          lost: false,
        });
      }
      return json({ frames: [], rows: [old], highest_seq: 100, next_cursor: 100, lost: false, has_more: false });
    });
    const { result } = await open();
    await act(() => result.current.sendPrompt("batched send"));
    await flushAsync();
    expect(reads).toBe(1);
    expect(result.current.state.activity.map((row) => row.id)).toEqual([old.id, id, reply.id]);
    expect(result.current.state.optimisticRows).toEqual([]);
  });

  it("ignores an in-flight recovery from the previous session even when its fetch ignores abort", async () => {
    let id = "";
    let release!: (response: Response) => void;
    let started = false;
    installAcpFakes(({ url, body }) => {
      if (url.includes("/acp/prompt")) {
        id = JSON.parse(body!).prompt_id;
        return json({ disposition: "sent" }, 202);
      }
      if (!url.includes("/acp/replay")) return undefined;
      const params = new URL(url, "http://x").searchParams;
      if (id && params.get("since") === "100" && params.get("view") === "rows" && url.includes("prompt-recovery")) {
        started = true;
        return new Promise<Response>((resolve) => {
          release = resolve;
        });
      }
      return json({ frames: [], rows: [old], lost: false, highest_seq: 100, next_cursor: 100, has_more: false });
    });
    const hook = renderHook(({ sid }) => useAcpSession(sid), {
      initialProps: { sid: "prompt-recovery" },
      wrapper: profileWrapper,
    });
    await flushAsync();
    lastSocket().open();
    await flushAsync();
    await act(() => hook.result.current.sendPrompt("from the old session"));
    lastSocket().message({
      session_id: "prompt-recovery",
      seq: 101,
      event: { UserPromptSent: { prompt_id: id, text: "from the old session" } },
    });
    lastSocket().message({ session_id: "prompt-recovery", seq: 102, event: { AgentMessageChunk: { text: "reply" } } });
    lastSocket().message({ kind: "transcript_delta", delta: { Append: reply } });
    await flushAsync();
    expect(started).toBe(true);
    hook.rerender({ sid: "new-session" });
    await flushAsync();
    lastSocket().open();
    await flushAsync();
    const before = hook.result.current.state;
    release(
      json({
        rows: [{ id, kind: "user_prompt", text: "from the old session", at: "t" }, reply],
        highest_seq: 102,
        lost: false,
      }),
    );
    await flushAsync();
    expect(hook.result.current.state.activity).toEqual(before.activity);
    expect(hook.result.current.state.optimisticRows).toEqual([]);
  });

  it("recovers before a genuine tool-start successor attested by a post-prompt raw event", async () => {
    let id = "";
    const tool = { id: "new-tool", name: "Read", kind: "read", args_preview: "{}", started_at: "2026-01-01T00:00:02Z" };
    const start = { id: "start-new-tool", kind: "tool_start", tool_call_id: tool.id, tool, text: "Read", at: "t" };
    const done = { id: "done-new-tool", kind: "tool_complete", tool_call_id: tool.id, text: "done", at: "t" };
    installAcpFakes(({ url, body }) => {
      if (url.includes("/acp/prompt")) {
        id = JSON.parse(body!).prompt_id;
        return json({ disposition: "sent" }, 202);
      }
      if (!url.includes("/acp/replay")) return undefined;
      const params = new URL(url, "http://x").searchParams;
      if (id && params.get("since") === "100" && params.get("view") === "rows")
        return json({
          rows: [{ id, kind: "user_prompt", text: "read next", at: "t" }, start, done],
          highest_seq: 103,
          lost: false,
        });
      return json({ frames: [], rows: [old], highest_seq: 100, next_cursor: 100, has_more: false, lost: false });
    });
    const { result } = await open();
    await act(() => result.current.sendPrompt("read next"));
    lastSocket().message({
      session_id: "prompt-recovery",
      seq: 101,
      event: { UserPromptSent: { prompt_id: id, text: "read next" } },
    });
    lastSocket().message({ session_id: "prompt-recovery", seq: 102, event: { ToolCallStarted: { tool_call: tool } } });
    lastSocket().message({ kind: "transcript_delta", delta: { Append: start } });
    lastSocket().message({
      session_id: "prompt-recovery",
      seq: 103,
      event: { ToolCallCompleted: { tool_call_id: tool.id, is_error: false, content: "done" } },
    });
    lastSocket().message({ kind: "transcript_delta", delta: { Append: done } });
    await flushAsync();
    expect(result.current.state.activity.map((row) => row.id)).toEqual([old.id, id, start.id, done.id]);
    expect(result.current.state.optimisticRows).toEqual([]);
  });

  it.each(["live", "replay", "snapshot", "cache"] as const)(
    "does not move a recovered prompt ahead of a repeated tool start from %s",
    async (source) => {
      let id = "";
      let reads = 0;
      let release!: (response: Response) => void;
      const held = new Promise<Response>((resolve) => {
        release = resolve;
      });
      const tool = {
        id: "earlier-tool",
        name: "Read",
        kind: "read",
        args_preview: "{}",
        started_at: "2026-01-01T00:00:00Z",
      };
      const frame = { session_id: "prompt-recovery", seq: 100, event: { ToolCallStarted: { tool_call: tool } } };
      const start = {
        id: "start-earlier-tool",
        kind: "tool_start",
        tool_call_id: tool.id,
        tool,
        text: "Read",
        at: "t",
      };
      const done = { id: "done-earlier-tool", kind: "tool_complete", tool_call_id: tool.id, text: "done", at: "t" };
      if (source === "cache") {
        localStorage.setItem(
          "aoe:acp-state:v1:prompt-recovery",
          JSON.stringify({
            savedAt: Date.now(),
            state: {
              ...emptyAcpState(),
              lastSeq: 100,
              oldestSeq: 1,
              activity: [old, { ...start, toolCallId: tool.id }],
            },
          }),
        );
      }
      const calls = installAcpFakes(({ url, body }) => {
        if (url.includes("/acp/prompt")) {
          id = JSON.parse(body!).prompt_id;
          return json({ disposition: "sent" }, 202);
        }
        if (!url.includes("/acp/replay")) return undefined;
        const params = new URL(url, "http://x").searchParams;
        if (id && params.get("since") === "100" && params.get("view") === "rows") {
          reads += 1;
          return held;
        }
        const rows = [old];
        if (source === "replay") rows.push(start);
        return json({
          frames: source === "replay" ? [frame] : [],
          rows,
          highest_seq: 100,
          next_cursor: 100,
          has_more: false,
          lost: false,
        });
      });
      const { result, unmount } = await open();
      try {
        if (source === "live") lastSocket().message(frame);
        if (source === "live" || source === "snapshot") {
          lastSocket().message({ kind: "transcript_snapshot", rows: [start] });
        }
        await waitFor(() => expect(result.current.state.activity.map((row) => row.id)).toEqual([old.id, start.id]));
        await act(() => result.current.sendPrompt("steer the running turn"));
        lastSocket().message({
          session_id: "prompt-recovery",
          seq: 101,
          event: { UserPromptSent: { prompt_id: id, text: "steer the running turn" } },
        });
        lastSocket().message({ ...frame, seq: 102 });
        lastSocket().message({ kind: "transcript_delta", delta: { Patch: { id: start.id, row: start } } });
        lastSocket().message({
          session_id: "prompt-recovery",
          seq: 103,
          event: { ToolCallCompleted: { tool_call_id: tool.id, is_error: false, content: "done" } },
        });
        lastSocket().message({ kind: "transcript_delta", delta: { Append: done } });
        await waitFor(() => expect(reads).toBe(1));
        await act(async () =>
          release(
            json({
              rows: [{ id, kind: "user_prompt", text: "steer the running turn", at: "t" }, start, done],
              highest_seq: 103,
              lost: false,
            }),
          ),
        );
        await waitFor(() => expect(result.current.state.optimisticRows).toEqual([]));
        expect(result.current.state.activity.map((row) => row.id)).toEqual([old.id, start.id, id, done.id]);
        expect(calls.filter((call) => call.method === "POST" && call.url.includes("/acp/prompt"))).toHaveLength(1);
      } finally {
        unmount();
      }
    },
  );

  it.each([
    ["compaction_summary", "compaction-summary-c"],
    ["tool_complete", "done-earlier-tool"],
  ])("keeps a patched pre-prompt %s out of successor candidates", async (kind, stableId) => {
    let id = "";
    const existing = { id: stableId, kind, text: "before the prompt", at: "t" };
    const patched = { ...existing, text: "updated after the prompt" };
    const calls = installAcpFakes(({ url, body }) => {
      if (url.includes("/acp/prompt")) {
        id = JSON.parse(body!).prompt_id;
        return json({ disposition: "sent" }, 202);
      }
      if (!url.includes("/acp/replay")) return undefined;
      const params = new URL(url, "http://x").searchParams;
      if (id && params.get("since") === "100" && params.get("view") === "rows")
        return json({
          rows: [{ id, kind: "user_prompt", text: "after the stable row", at: "t" }, patched, reply],
          highest_seq: 103,
          lost: false,
        });
      return json({ frames: [], rows: [old], highest_seq: 90, next_cursor: 90, has_more: false, lost: false });
    });
    const { result, unmount } = await open();
    await act(() => result.current.sendPrompt("after the stable row"));
    // Deliver the old row and the ACK in one React batch, before activityRef commits.
    act(() => {
      const socket = lastSocket();
      for (const data of [
        { session_id: "prompt-recovery", seq: 90, event: { AgentMessageChunk: { text: "old" } } },
        { kind: "transcript_delta", delta: { Append: existing } },
        {
          session_id: "prompt-recovery",
          seq: 101,
          event: { UserPromptSent: { prompt_id: id, text: "after the stable row" } },
        },
        { session_id: "prompt-recovery", seq: 102, event: { AgentMessageChunk: { text: "patch" } } },
        { kind: "transcript_delta", delta: { Patch: { id: stableId, row: patched } } },
        { session_id: "prompt-recovery", seq: 103, event: { AgentMessageChunk: { text: reply.text } } },
        { kind: "transcript_delta", delta: { Append: reply } },
      ])
        socket.onmessage?.({ data: JSON.stringify(data) } as MessageEvent);
    });
    await waitFor(() => expect(result.current.state.optimisticRows).toEqual([]));
    expect(result.current.state.activity.map((row) => row.id)).toEqual([old.id, stableId, id, reply.id]);
    expect(result.current.state.activity.find((row) => row.id === stableId)?.text).toBe(patched.text);
    expect(calls.filter((call) => call.method === "POST" && call.url.includes("/acp/prompt"))).toHaveLength(1);
    unmount();
  });

  it.each(["replay", "snapshot", "cache"] as const)(
    "preserves pre-prompt stable IDs loaded from %s",
    async (source) => {
      let id = "";
      const existing = { id: "compaction-summary-before", kind: "compaction_summary", text: "before", at: "t" };
      if (source === "cache") {
        localStorage.setItem(
          "aoe:acp-state:v1:prompt-recovery",
          JSON.stringify({
            savedAt: Date.now(),
            state: { ...emptyAcpState(), lastSeq: 100, oldestSeq: 1, activity: [old, existing] },
          }),
        );
      }
      installAcpFakes(({ url, body }) => {
        if (url.includes("/acp/prompt")) {
          id = JSON.parse(body!).prompt_id;
          return json({ disposition: "sent" }, 202);
        }
        if (!url.includes("/acp/replay")) return undefined;
        const params = new URL(url, "http://x").searchParams;
        if (id && params.get("since") === "100" && params.get("view") === "rows")
          return json({
            rows: [{ id, kind: "user_prompt", text: "follow-up", at: "t" }, existing, reply],
            highest_seq: 102,
            lost: false,
          });
        let rows = [old];
        if (source === "replay") rows = [old, existing];
        return json({ frames: [], rows, highest_seq: 100, next_cursor: 100, has_more: false, lost: false });
      });
      const hook = await open();
      if (source === "snapshot") lastSocket().message({ kind: "transcript_snapshot", rows: [existing] });
      await flushAsync();
      expect(hook.result.current.state.activity.map((row) => row.id)).toEqual([old.id, existing.id]);
      await act(() => hook.result.current.sendPrompt("follow-up"));
      lastSocket().message({
        session_id: "prompt-recovery",
        seq: 101,
        event: { UserPromptSent: { prompt_id: id, text: "follow-up" } },
      });
      lastSocket().message({
        session_id: "prompt-recovery",
        seq: 102,
        event: { AgentMessageChunk: { text: reply.text } },
      });
      lastSocket().message({
        kind: "transcript_delta",
        delta: { Patch: { id: existing.id, row: { ...existing, text: "updated" } } },
      });
      lastSocket().message({ kind: "transcript_delta", delta: { Append: reply } });
      await waitFor(() => expect(hook.result.current.state.optimisticRows).toEqual([]));
      expect(hook.result.current.state.activity.map((row) => row.id)).toEqual([old.id, existing.id, id, reply.id]);
      hook.unmount();
    },
  );

  it("retains the pre-prompt provenance of rows loaded by older-history paging", async () => {
    let id = "";
    let release!: (response: Response) => void;
    const held = new Promise<Response>((resolve) => {
      release = resolve;
    });
    let recovering = false;
    const earlier = { id: "earlier-summary", kind: "compaction_summary", text: "earlier", at: "t" };
    const boundary = { id: "earlier-prompt", kind: "user_prompt", text: "previous turn", at: "t" };
    installAcpFakes(({ url, body }) => {
      if (url.includes("/acp/prompt")) {
        id = JSON.parse(body!).prompt_id;
        return json({ disposition: "sent" }, 202);
      }
      if (!url.includes("/acp/replay")) return undefined;
      const params = new URL(url, "http://x").searchParams;
      if (params.get("before") === "80")
        return json({ rows: [earlier], highest_seq: 100, next_cursor: 1, has_more: false, lost: false });
      if (id && params.get("since") === "100" && params.get("view") === "rows") {
        recovering = true;
        return held;
      }
      return json({
        frames: [],
        rows: [boundary, old],
        highest_seq: 100,
        next_cursor: 80,
        has_more: true,
        lost: false,
      });
    });
    const hook = await open();
    await act(() => hook.result.current.sendPrompt("after earlier history"));
    lastSocket().message({
      session_id: "prompt-recovery",
      seq: 101,
      event: { UserPromptSent: { prompt_id: id, text: "after earlier history" } },
    });
    await act(() => hook.result.current.loadOlder());
    expect(hook.result.current.state.activity.map((row) => row.id)).toEqual([earlier.id, boundary.id, old.id]);
    lastSocket().message({ session_id: "prompt-recovery", seq: 102, event: { ThinkingStarted: {} } });
    lastSocket().message({
      kind: "transcript_delta",
      delta: { Patch: { id: earlier.id, row: { ...earlier, text: "patched earlier summary" } } },
    });
    lastSocket().message({
      session_id: "prompt-recovery",
      seq: 103,
      event: { AgentMessageChunk: { text: reply.text } },
    });
    lastSocket().message({ kind: "transcript_delta", delta: { Append: reply } });
    await waitFor(() => expect(recovering).toBe(true));
    await act(async () =>
      release(
        json({
          rows: [{ id, kind: "user_prompt", text: "after earlier history", at: "t" }, earlier, reply],
          highest_seq: 103,
          lost: false,
        }),
      ),
    );
    await waitFor(() => expect(hook.result.current.state.optimisticRows).toEqual([]));
    expect(hook.result.current.state.activity.map((row) => row.id)).toEqual([
      earlier.id,
      boundary.id,
      old.id,
      id,
      reply.id,
    ]);
    hook.unmount();
  });

  it("ignores older-history completion from a previous session", async () => {
    let release!: (response: Response) => void;
    const held = new Promise<Response>((resolve) => {
      release = resolve;
    });
    let started = false;
    const boundary = { id: "boundary", kind: "user_prompt", text: "previous", at: "t" };
    installAcpFakes(({ url }) => {
      if (!url.includes("/acp/replay")) return undefined;
      const params = new URL(url, "http://x").searchParams;
      if (url.includes("prompt-recovery") && params.get("before") === "80") {
        started = true;
        return held;
      }
      let hasMore = false;
      if (url.includes("prompt-recovery")) hasMore = true;
      return json({
        frames: [],
        rows: [boundary, old],
        highest_seq: 100,
        next_cursor: 80,
        has_more: hasMore,
        lost: false,
      });
    });
    const hook = renderHook(({ sid }) => useAcpSession(sid), {
      initialProps: { sid: "prompt-recovery" },
      wrapper: profileWrapper,
    });
    await flushAsync();
    lastSocket().open();
    await flushAsync();
    let loading!: Promise<void>;
    act(() => {
      loading = hook.result.current.loadOlder();
    });
    await waitFor(() => expect(started).toBe(true));
    hook.rerender({ sid: "other-session" });
    await flushAsync();
    lastSocket().open();
    await flushAsync();
    const before = hook.result.current.state.activity;
    expect(hook.result.current.hasMoreOlder).toBe(false);
    await act(async () => {
      release(
        json({
          rows: [{ ...old, id: "stale-history" }],
          next_cursor: 1,
          has_more: true,
          highest_seq: 100,
          lost: false,
        }),
      );
      await loading;
    });
    expect(hook.result.current.state.activity).toEqual(before);
    expect(hook.result.current.hasMoreOlder).toBe(false);
    hook.unmount();
  });

  it("re-arms exhausted recovery only when a genuinely new successor row arrives", async () => {
    vi.useFakeTimers();
    let id = "";
    let reads = 0;
    let successor = false;
    const calls = installAcpFakes(({ url, body }) => {
      if (url.includes("/acp/prompt")) {
        id = JSON.parse(body!).prompt_id;
        return json({ disposition: "sent" }, 202);
      }
      if (!url.includes("/acp/replay")) return undefined;
      const params = new URL(url, "http://x").searchParams;
      if (id && params.get("view") === "rows") {
        reads += 1;
        const rows = [{ id, kind: "user_prompt", text: "wait for output", at: "t" }];
        if (successor) rows.push(reply);
        return json({ rows, lost: false, highest_seq: 104 });
      }
      return json({ frames: [], rows: [old], highest_seq: 100, next_cursor: 100, has_more: false, lost: false });
    });
    const hook = await open();
    await act(() => hook.result.current.sendPrompt("wait for output"));
    lastSocket().message({
      session_id: "prompt-recovery",
      seq: 101,
      event: { UserPromptSent: { prompt_id: id, text: "wait for output" } },
    });
    lastSocket().message({ session_id: "prompt-recovery", seq: 102, event: { ThinkingStarted: {} } });
    await flushAsync();
    expect(reads).toBe(1);
    for (const delay of [1000, 2000, 4000]) {
      await act(() => vi.advanceTimersByTimeAsync(delay));
      await flushAsync();
    }
    expect(reads).toBe(3);
    lastSocket().message({ session_id: "prompt-recovery", seq: 103, event: { ThinkingEnded: {} } });
    lastSocket().message({
      kind: "transcript_delta",
      delta: { Patch: { id: old.id, row: { ...old, text: "old patch" } } },
    });
    await flushAsync();
    expect(reads).toBe(3);
    successor = true;
    lastSocket().message({
      session_id: "prompt-recovery",
      seq: 104,
      event: { AgentMessageChunk: { text: reply.text } },
    });
    lastSocket().message({ kind: "transcript_delta", delta: { Append: reply } });
    await flushAsync();
    expect(reads).toBe(4);
    expect(hook.result.current.state.activity.map((row) => row.id)).toEqual([old.id, id, reply.id]);
    expect(hook.result.current.state.optimisticRows).toEqual([]);
    expect(calls.filter((call) => call.method === "POST" && call.url.includes("/acp/prompt"))).toHaveLength(1);
    hook.unmount();
  });

  it("bounds successor-triggered recovery even while new rows keep streaming", async () => {
    vi.useFakeTimers();
    let id = "";
    let reads = 0;
    installAcpFakes(({ url, body }) => {
      if (url.includes("/acp/prompt")) {
        id = JSON.parse(body!).prompt_id;
        return json({ disposition: "sent" }, 202);
      }
      if (!url.includes("/acp/replay")) return undefined;
      if (id && new URL(url, "http://x").searchParams.get("view") === "rows") {
        reads += 1;
        return json({ rows: [], lost: false, highest_seq: 200 });
      }
      return json({ frames: [], rows: [old], highest_seq: 100, next_cursor: 100, has_more: false, lost: false });
    });
    const hook = await open();
    await act(() => hook.result.current.sendPrompt("keep this missing prompt"));
    lastSocket().message({
      session_id: "prompt-recovery",
      seq: 101,
      event: { UserPromptSent: { prompt_id: id, text: "keep this missing prompt" } },
    });
    lastSocket().message({ session_id: "prompt-recovery", seq: 102, event: { ThinkingStarted: {} } });
    await flushAsync();
    await act(() => vi.advanceTimersByTimeAsync(7000));
    expect(reads).toBe(3);
    for (let seq = 103; seq < 108; seq += 1) {
      lastSocket().message({ session_id: "prompt-recovery", seq, event: { AgentMessageChunk: { text: "more" } } });
      lastSocket().message({
        kind: "transcript_delta",
        delta: { Append: { ...reply, id: `msg-${seq}` } },
      });
      await flushAsync();
      await act(() => vi.advanceTimersByTimeAsync(7000));
    }
    expect(reads).toBe(6);
    expect(hook.result.current.state.optimisticRows).toMatchObject([{ id, text: "keep this missing prompt" }]);
    hook.unmount();
  });

  it("bounds failed recovery to three attempts and never resends or hides the prompt", async () => {
    vi.useFakeTimers();
    let id = "";
    let reads = 0;
    const calls = installAcpFakes(({ url, body }) => {
      if (url.includes("/acp/prompt")) {
        id = JSON.parse(body!).prompt_id;
        return json({ disposition: "sent" }, 202);
      }
      if (id && url.includes("/acp/replay") && new URL(url, "http://x").searchParams.get("view") === "rows") {
        reads += 1;
        return new Response("unavailable", { status: 500 });
      }
      return undefined;
    });
    const { result } = await open();
    await act(() => result.current.sendPrompt("keep my text"));
    lastSocket().message({
      session_id: "prompt-recovery",
      seq: 1,
      event: { UserPromptSent: { prompt_id: id, text: "keep my text" } },
    });
    lastSocket().message({ session_id: "prompt-recovery", seq: 2, event: { AgentMessageChunk: { text: "reply" } } });
    lastSocket().message({ kind: "transcript_delta", delta: { Append: reply } });
    await flushAsync();
    expect(reads).toBe(1);
    for (const [delay, expected] of [
      [1000, 2],
      [2000, 3],
      [4000, 3],
    ]) {
      await act(() => vi.advanceTimersByTimeAsync(delay!));
      await flushAsync();
      expect(reads).toBe(expected);
    }
    expect(result.current.state.optimisticRows).toMatchObject([{ id, text: "keep my text" }]);
    expect(calls.filter((call) => call.method === "POST" && call.url.includes("/acp/prompt"))).toHaveLength(1);
  });

  it.each([false, true])(
    "restores the canonical prompt before its reply without resending (late ACK: %s)",
    async (lateAck) => {
      let promptId = "";
      let highestSeq = 100;
      const canonical = () => ({
        id: promptId,
        kind: "user_prompt",
        text: "Follow-up",
        attachments: [attachment],
        at: "2026-01-01T00:00:01Z",
      });
      const calls = installAcpFakes(({ url, body }) => {
        if (url.includes("/acp/prompt")) {
          promptId = JSON.parse(body!).prompt_id;
          return json({ disposition: "sent", seq: 101 }, 202);
        }
        if (!url.includes("/acp/replay")) return undefined;
        const params = new URL(url, "http://x").searchParams;
        if (promptId && params.get("since") === "100" && params.get("view") === "rows")
          return json({
            frames: [],
            rows: [canonical(), reply],
            lost: false,
            highest_seq: highestSeq,
            next_cursor: highestSeq,
            has_more: false,
          });
        return json({ frames: [], rows: [old], lost: false, highest_seq: 100, next_cursor: 100, has_more: false });
      });
      const { result } = await open();
      await act(() => result.current.sendPrompt("Follow-up"));
      const ack = () =>
        lastSocket().message({
          session_id: "prompt-recovery",
          seq: 101,
          event: { UserPromptSent: { prompt_id: promptId, text: "Follow-up", attachments: [attachment] } },
        });
      if (!lateAck) ack();
      highestSeq = 102;
      lastSocket().message({
        session_id: "prompt-recovery",
        seq: 102,
        event: { AgentMessageChunk: { text: reply.text } },
      });
      lastSocket().message({ kind: "transcript_delta", seq: 102, delta: { Append: reply } });
      if (lateAck) {
        highestSeq = 103;
        lastSocket().message({ session_id: "prompt-recovery", seq: 103, event: { Stopped: { reason: "done" } } });
        ack();
      }
      await flushAsync(30);
      expect(result.current.state.activity.map((row) => row.id)).toEqual([old.id, promptId, reply.id]);
      expect(result.current.state.optimisticRows).toEqual([]);
      expect(result.current.state.activity.find((row) => row.id === promptId)?.attachments?.[0]).toMatchObject({
        id: attachment.id,
        url: `/api/sessions/prompt-recovery/acp/attachments/${attachment.id}`,
      });
      expect(calls.filter((call) => call.url.includes("/acp/prompt") && call.method === "POST")).toHaveLength(1);
      if (lateAck) expect(result.current.state.turnActive).toBe(false);
      lastSocket().message({ kind: "transcript_delta", seq: 101, delta: { Append: canonical() } });
      await flushAsync();
      expect(result.current.state.activity.map((row) => row.id)).toEqual([old.id, promptId, reply.id]);
    },
  );
});

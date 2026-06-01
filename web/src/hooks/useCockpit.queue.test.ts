// @vitest-environment jsdom
//
// Reducer tests for the client-side prompt-queue feature (#1031),
// plus hook-level drain-race regression tests for #1144 (queued
// follow-ups silently dropped on reconnect).
//
// While a turn is running, sendPrompt dispatches `enqueue_prompt`
// instead of the immediate POST path. The reducer keeps the queue
// across re-renders so the drain effect can pop heads on Stopped, and
// the QueuedPromptsStrip can render / edit / drop entries before they
// fire.

import { act, renderHook } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { emptyCockpitState, type QueuedPrompt } from "../lib/cockpitTypes";
import { AgentProfileProvider } from "../lib/agentProfileContext";
import { cockpitHookReducer, combineQueuedPrompts, useCockpit } from "./useCockpit";

describe("cockpitHookReducer / queue actions", () => {
  it("emptyCockpitState starts with an empty queue", () => {
    expect(emptyCockpitState().queuedPrompts).toEqual([]);
  });

  it("enqueue_prompt appends to the end of queuedPrompts", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
    try {
      const s1 = cockpitHookReducer(emptyCockpitState(), {
        kind: "enqueue_prompt",
        text: "first",
      });
      const s2 = cockpitHookReducer(s1, {
        kind: "enqueue_prompt",
        text: "second",
      });
      expect(s2.queuedPrompts).toHaveLength(2);
      expect(s2.queuedPrompts[0]?.text).toBe("first");
      expect(s2.queuedPrompts[1]?.text).toBe("second");
      expect(s2.queuedPrompts[0]?.queuedAt).toBe(
        "2026-01-01T00:00:00.000Z",
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it("dequeue_prompt removes the matching entry by id", () => {
    const s1 = cockpitHookReducer(emptyCockpitState(), {
      kind: "enqueue_prompt",
      text: "first",
    });
    const s2 = cockpitHookReducer(s1, {
      kind: "enqueue_prompt",
      text: "second",
    });
    const headId = s2.queuedPrompts[0]?.id;
    expect(headId).toBeDefined();
    const s3 = cockpitHookReducer(s2, {
      kind: "dequeue_prompt",
      id: headId!,
    });
    expect(s3.queuedPrompts).toHaveLength(1);
    expect(s3.queuedPrompts[0]?.text).toBe("second");
  });

  it("dequeue_prompt is a no-op for a missing id", () => {
    const s1 = cockpitHookReducer(emptyCockpitState(), {
      kind: "enqueue_prompt",
      text: "first",
    });
    const s2 = cockpitHookReducer(s1, {
      kind: "dequeue_prompt",
      id: "nope",
    });
    expect(s2.queuedPrompts).toHaveLength(1);
  });

  it("edit_queued_prompt updates only the targeted entry's text", () => {
    const s1 = cockpitHookReducer(emptyCockpitState(), {
      kind: "enqueue_prompt",
      text: "first",
    });
    const s2 = cockpitHookReducer(s1, {
      kind: "enqueue_prompt",
      text: "second",
    });
    const targetId = s2.queuedPrompts[1]?.id;
    expect(targetId).toBeDefined();
    const s3 = cockpitHookReducer(s2, {
      kind: "edit_queued_prompt",
      id: targetId!,
      text: "second (edited)",
    });
    expect(s3.queuedPrompts[0]?.text).toBe("first");
    expect(s3.queuedPrompts[1]?.text).toBe("second (edited)");
  });

  it("clear_queue drops every entry", () => {
    const s1 = cockpitHookReducer(emptyCockpitState(), {
      kind: "enqueue_prompt",
      text: "first",
    });
    const s2 = cockpitHookReducer(s1, {
      kind: "enqueue_prompt",
      text: "second",
    });
    const s3 = cockpitHookReducer(s2, { kind: "clear_queue" });
    expect(s3.queuedPrompts).toEqual([]);
  });

  it("dequeue_prompts_by_id removes only the listed ids, preserving order", () => {
    const s1 = cockpitHookReducer(emptyCockpitState(), {
      kind: "enqueue_prompt",
      text: "first",
    });
    const s2 = cockpitHookReducer(s1, {
      kind: "enqueue_prompt",
      text: "second",
    });
    const s3 = cockpitHookReducer(s2, {
      kind: "enqueue_prompt",
      text: "third",
    });
    const firstId = s3.queuedPrompts[0]!.id;
    const thirdId = s3.queuedPrompts[2]!.id;
    const s4 = cockpitHookReducer(s3, {
      kind: "dequeue_prompts_by_id",
      ids: [firstId, thirdId],
    });
    expect(s4.queuedPrompts).toHaveLength(1);
    expect(s4.queuedPrompts[0]?.text).toBe("second");
  });

  it("dequeue_prompts_by_id with an empty id list is a no-op", () => {
    const s1 = cockpitHookReducer(emptyCockpitState(), {
      kind: "enqueue_prompt",
      text: "first",
    });
    const s2 = cockpitHookReducer(s1, {
      kind: "dequeue_prompts_by_id",
      ids: [],
    });
    expect(s2).toBe(s1);
  });

  it("queue is independent of activity / turnActive state", () => {
    // Enqueue while a turn is mid-flight (turnActive=true, activity has
    // a user_prompt row) and ensure the queue mutation does not clobber
    // the rest of state.
    const base = {
      ...emptyCockpitState(),
      activity: [
        {
          id: "user-1",
          kind: "user_prompt" as const,
          text: "original",
          at: "2026-01-01T00:00:00Z",
        },
      ],
      turnActive: true,
    };
    const next = cockpitHookReducer(base, {
      kind: "enqueue_prompt",
      text: "queued follow-up",
    });
    expect(next.activity).toEqual(base.activity);
    expect(next.turnActive).toBe(true);
    expect(next.queuedPrompts[0]?.text).toBe("queued follow-up");
  });
});

describe("combineQueuedPrompts (combined drain mode)", () => {
  const mk = (id: string, text: string): QueuedPrompt => ({
    id,
    text,
    queuedAt: "2026-01-01T00:00:00.000Z",
  });

  it("joins entries with a blank line", () => {
    const out = combineQueuedPrompts([
      mk("a", "first"),
      mk("b", "second"),
      mk("c", "third"),
    ]);
    expect(out).toBe("first\n\nsecond\n\nthird");
  });

  it("preserves intra-entry newlines unchanged", () => {
    const out = combineQueuedPrompts([
      mk("a", "line 1\nline 2"),
      mk("b", "after"),
    ]);
    expect(out).toBe("line 1\nline 2\n\nafter");
  });

  it("returns an empty string for an empty queue", () => {
    expect(combineQueuedPrompts([])).toBe("");
  });

  it("returns a single entry unchanged for a one-item queue", () => {
    expect(combineQueuedPrompts([mk("a", "only one")])).toBe("only one");
  });
});

// Hook-level regression tests for #1144: queued prompts were silently
// dropped when the drain effect fired during the WS reconnect window.
// connect() awaits fetchReplay BEFORE opening the WS, and replay can
// dispatch a Stopped frame (turnActive flips to false). Under the prior
// optimistic-clear ordering, the drain effect would fire while status
// was still "connecting", clear the queue, then dispatchPromptNow would
// bail with an error banner -- queue gone, message never sent.

interface FakeSocket {
  url: string;
  protocols: string[] | string | undefined;
  readyState: number;
  onopen: ((ev: Event) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
  onerror: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  close: () => void;
  send: (data: string | ArrayBufferLike | Blob | ArrayBufferView) => void;
}

const sockets: FakeSocket[] = [];
let originalWebSocket: typeof WebSocket;

class FakeWebSocket implements FakeSocket {
  url: string;
  protocols: string[] | string | undefined;
  readyState: number = 0;
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  constructor(url: string, protocols?: string | string[]) {
    this.url = url;
    this.protocols = protocols;
    sockets.push(this);
  }
  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
    if (this.onclose) {
      this.onclose({
        code: 1000,
        reason: "test close",
        wasClean: true,
      } as CloseEvent);
    }
  }
  send(): void {
    /* no-op */
  }
}

async function flushAsync(): Promise<void> {
  await act(async () => {
    for (let i = 0; i < 8; i++) {
      await Promise.resolve();
    }
  });
}

describe("useCockpit drain race (#1144)", () => {
  let promptPostCount: number;
  let promptPostStatus: number;
  let promptPostBody: string;
  let promptPostBodies: string[];
  let replayResponse: { frames: unknown[]; lost: boolean; highest_seq: number };

  beforeEach(() => {
    sockets.length = 0;
    promptPostCount = 0;
    promptPostStatus = 200;
    promptPostBody = "simulated failure";
    promptPostBodies = [];
    replayResponse = { frames: [], lost: false, highest_seq: 0 };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : input.toString();
        if (url.includes("/cockpit/replay")) {
          return new Response(JSON.stringify(replayResponse), { status: 200 });
        }
        if (url.includes("/cockpit/prompt")) {
          promptPostCount += 1;
          if (typeof init?.body === "string") {
            promptPostBodies.push(init.body);
          }
          if (promptPostStatus >= 400) {
            return new Response(promptPostBody, { status: promptPostStatus });
          }
          return new Response("{}", { status: promptPostStatus });
        }
        return new Response("{}", { status: 200 });
      }),
    );
    originalWebSocket = global.WebSocket;
    global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    global.WebSocket = originalWebSocket;
    vi.unstubAllGlobals();
  });

  it("enqueues without POSTing when sendPrompt is called while the WS is still connecting (#1359)", async () => {
    const { result } = renderHook(() => useCockpit("sess-drain-1"));
    await flushAsync();
    expect(sockets).toHaveLength(1);
    // WS is still in CONNECTING (FakeWebSocket starts at readyState=0).
    // sendPrompt should not POST (drain effect is gated on status ===
    // "open"), and per #1359 it should also not drop the message: park
    // it in the queue so the drain effect can fire it once the socket
    // reopens.
    act(() => {
      void result.current.sendPrompt("queued before open");
    });
    await flushAsync();
    expect(promptPostCount).toBe(0);
    expect(result.current.state.queuedPrompts).toHaveLength(1);
    expect(result.current.state.queuedPrompts[0]?.text).toBe(
      "queued before open",
    );
  });

  it("drains the queue once the WS opens after an inactive-state enqueue (#1359)", async () => {
    const { result } = renderHook(() => useCockpit("sess-drain-resume"));
    await flushAsync();
    const ws = sockets[0]!;
    // Enqueue while WS is still CONNECTING: per #1359 sendPrompt parks
    // the entry rather than erroring.
    act(() => {
      void result.current.sendPrompt("parked while offline");
    });
    await flushAsync();
    expect(promptPostCount).toBe(0);
    expect(result.current.state.queuedPrompts).toHaveLength(1);

    // Open the socket; the drain effect should now POST the parked
    // entry and clear the queue.
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();
    expect(promptPostCount).toBe(1);
    expect(promptPostBodies[0]).toContain("parked while offline");
    expect(result.current.state.queuedPrompts).toEqual([]);
  });

  it("enqueues when sendPrompt is called while workerStopped is true (#1359)", async () => {
    const { result } = renderHook(() => useCockpit("sess-stopped"));
    await flushAsync();
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();

    // Push a `Stopped { reason: "user_stopped" }` frame so the reducer
    // sets workerStopped=true. The drain effect parks on that flag, and
    // per #1359 sendPrompt mirrors the same guard so user-typed
    // messages also park instead of POSTing into a stopped worker.
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-stopped",
          seq: 1,
          event: { Stopped: { reason: "user_stopped" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    expect(result.current.state.workerStopped).toBe(true);

    act(() => {
      void result.current.sendPrompt("typed while stopped");
    });
    await flushAsync();
    expect(promptPostCount).toBe(0);
    expect(result.current.state.queuedPrompts).toHaveLength(1);
    expect(result.current.state.queuedPrompts[0]?.text).toBe(
      "typed while stopped",
    );
  });

  it("a fresh prompt POSTs (wakes) instead of parking when the worker is idle-dormant (#1689)", async () => {
    // workerState="absent": the reconciler reaped the worker for
    // inactivity. The REST poll reads "absent" until the respawn lands.
    const { result } = renderHook(() => useCockpit("sess-idle-fresh", "absent"));
    await flushAsync();
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();

    // Control: a plain absent worker (cold resume, no idle_auto_stop)
    // still parks — that guard is unchanged.
    act(() => {
      void result.current.sendPrompt("typed during cold resume");
    });
    await flushAsync();
    expect(promptPostCount).toBe(0);
    expect(result.current.state.queuedPrompts).toHaveLength(1);
    act(() => {
      void result.current.removeQueuedPrompt(
        result.current.state.queuedPrompts[0]!.id,
      );
    });
    await flushAsync();

    // The daemon publishes idle_auto_stop: the worker is dormant and a
    // prompt POST is the wake path. A freshly-typed prompt must POST
    // directly (the server clears dormancy + respawns + delivers)
    // rather than parking in the local queue forever — the bug.
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-idle-fresh",
          seq: 1,
          event: { Stopped: { reason: "idle_auto_stop" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    expect(result.current.state.workerIdleStopped).toBe(true);

    act(() => {
      void result.current.sendPrompt("wake me up");
    });
    await flushAsync();
    expect(promptPostCount).toBe(1);
    expect(promptPostBodies[0]).toContain("wake me up");
    expect(result.current.state.queuedPrompts).toEqual([]);
  });

  it("drains a prompt parked before idle_auto_stop once dormancy lands (#1689)", async () => {
    // The real stuck scenario: a prompt was queued while the worker was
    // a cold-absent resume, then the reconciler reaped it to dormant.
    // The dormancy signal must let the drain effect fire the parked
    // prompt (the wake POST), otherwise it sits queued forever.
    const { result } = renderHook(() => useCockpit("sess-idle-drain", "absent"));
    await flushAsync();
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();

    act(() => {
      void result.current.sendPrompt("parked before dormancy");
    });
    await flushAsync();
    expect(promptPostCount).toBe(0);
    expect(result.current.state.queuedPrompts).toHaveLength(1);

    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-idle-drain",
          seq: 1,
          event: { Stopped: { reason: "idle_auto_stop" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    expect(result.current.state.workerIdleStopped).toBe(true);
    expect(promptPostCount).toBe(1);
    expect(promptPostBodies[0]).toContain("parked before dormancy");
    expect(result.current.state.queuedPrompts).toEqual([]);
  });

  it("keeps an idle-dormant prompt queued without an error banner on a worker_not_ready 503 (#1748)", async () => {
    const { result } = renderHook(() => useCockpit("sess-idle-503", "absent"));
    await flushAsync();
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();

    // Worker reaped for inactivity: dormant.
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-idle-503",
          seq: 1,
          event: { Stopped: { reason: "idle_auto_stop" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    expect(result.current.state.workerIdleStopped).toBe(true);

    // The wake POST goes out, but the respawn did not finish within the
    // server's wait window, so it returns the typed retryable 503. The
    // prompt must NOT be dropped (it re-queues) and NO error banner shows;
    // the drain re-fires it once the worker comes online. See #1748.
    promptPostStatus = 503;
    promptPostBody = "worker_not_ready";
    await act(async () => {
      await result.current.sendPrompt("wake me up");
    });
    await flushAsync();

    expect(promptPostCount).toBe(1);
    expect(result.current.state.queuedPrompts).toHaveLength(1);
    expect(result.current.state.queuedPrompts[0]?.text).toBe("wake me up");
    expect(result.current.state.lastError ?? "").not.toContain(
      "Could not send prompt",
    );
  });

  it("still surfaces an error banner on a worker_capacity_full 503 (#1748)", async () => {
    // Control: the capacity 503 needs operator action, so unlike
    // worker_not_ready it must keep its banner rather than being silently
    // swallowed as a transient.
    const { result } = renderHook(() => useCockpit("sess-idle-cap", "absent"));
    await flushAsync();
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-idle-cap",
          seq: 1,
          event: { Stopped: { reason: "idle_auto_stop" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    expect(result.current.state.workerIdleStopped).toBe(true);

    promptPostStatus = 503;
    promptPostBody = "worker_capacity_full (4/4)";
    await act(async () => {
      await result.current.sendPrompt("wake me up");
    });
    await flushAsync();

    expect(result.current.state.lastError ?? "").toContain(
      "Could not send prompt (503)",
    );
  });

  it("keeps the error banner on a worker_not_ready 503 for an attachment send (#1748)", async () => {
    // Attachments cannot be re-queued (the local queue is text-only), so a
    // worker_not_ready 503 for an attachment send has no retry path. The
    // banner must show rather than being suppressed as transient.
    const { result } = renderHook(() => useCockpit("sess-idle-attach", "absent"));
    await flushAsync();
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-idle-attach",
          seq: 1,
          event: { Stopped: { reason: "idle_auto_stop" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    expect(result.current.state.workerIdleStopped).toBe(true);

    promptPostStatus = 503;
    promptPostBody = "worker_not_ready";
    await act(async () => {
      await result.current.sendPrompt("wake me up", [
        {
          kind: "image",
          mimeType: "image/png",
          dataB64: "aA==",
          name: "shot.png",
        },
      ]);
    });
    await flushAsync();

    expect(result.current.state.lastError ?? "").toContain(
      "Could not send prompt (503)",
    );
    expect(result.current.state.queuedPrompts).toEqual([]);
  });

  it("retires the optimistic turn when prompt POST is rejected with 4xx", async () => {
    const { result } = renderHook(() => useCockpit("sess-reject-4xx"));
    await flushAsync();
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();

    promptPostStatus = 400;
    await act(async () => {
      await result.current.sendPrompt("send bad attachment", [
        {
          kind: "image",
          mimeType: "image/x-xcf",
          dataB64: "aA==",
          name: "bad.xcf",
        },
      ]);
    });
    await flushAsync();

    expect(promptPostCount).toBe(1);
    expect(result.current.state.pendingUserPromptSeq).toBe(1);
    expect(result.current.state.lastStoppedSeq).toBe(1);
    expect(result.current.state.turnActive).toBe(false);
    expect(result.current.state.lastError).toContain(
      "Could not send prompt (400)",
    );
  });

  it("combined-mode drain leaves the queue intact when the prompt POST fails", async () => {
    const { result } = renderHook(() => useCockpit("sess-drain-2"));
    await flushAsync();
    const ws = sockets[0]!;
    // Open the socket so sendPrompt's status gate clears.
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();

    // Mark the turn as active so subsequent sendPrompt calls enqueue.
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-drain-2",
          seq: 1,
          event: { UserPromptSent: { text: "kicker" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    expect(result.current.state.turnActive).toBe(true);

    // Enqueue two follow-ups while the turn is active.
    act(() => {
      void result.current.sendPrompt("queued A");
    });
    act(() => {
      void result.current.sendPrompt("queued B");
    });
    await flushAsync();
    expect(result.current.state.queuedPrompts).toHaveLength(2);

    // Configure the prompt POST to fail, then end the turn so the drain
    // fires.
    promptPostStatus = 500;
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-drain-2",
          seq: 2,
          event: { Stopped: { reason: "prompt_complete" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();

    // The combined POST was attempted exactly once with both entries
    // joined, but failed; the queue MUST remain intact for the next
    // turn-end retry.
    expect(promptPostCount).toBe(1);
    expect(promptPostBodies[0]).toContain("queued A");
    expect(promptPostBodies[0]).toContain("queued B");
    expect(result.current.state.queuedPrompts).toHaveLength(2);
    expect(result.current.state.queuedPrompts[0]?.text).toBe("queued A");
    expect(result.current.state.queuedPrompts[1]?.text).toBe("queued B");
  });

  it("combined-mode drain only clears the items it sent, not items enqueued during the await", async () => {
    const { result } = renderHook(() => useCockpit("sess-drain-3"));
    await flushAsync();
    const ws = sockets[0]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();

    // Start a turn and enqueue two follow-ups.
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-drain-3",
          seq: 1,
          event: { UserPromptSent: { text: "kicker" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    act(() => {
      void result.current.sendPrompt("queued A");
    });
    act(() => {
      void result.current.sendPrompt("queued B");
    });
    await flushAsync();
    expect(result.current.state.queuedPrompts).toHaveLength(2);

    // Make the prompt POST hang so we have a window to enqueue more
    // entries during the await.
    let resolvePost: ((res: Response) => void) | null = null;
    const pendingPost = new Promise<Response>((resolve) => {
      resolvePost = resolve;
    });
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : input.toString();
        if (url.includes("/cockpit/replay")) {
          return new Response(
            JSON.stringify({ frames: [], lost: false, highest_seq: 0 }),
            { status: 200 },
          );
        }
        if (url.includes("/cockpit/prompt")) {
          promptPostCount += 1;
          if (typeof init?.body === "string") {
            promptPostBodies.push(init.body);
          }
          return pendingPost;
        }
        return new Response("{}", { status: 200 });
      }),
    );

    // End the turn -> drain fires, dispatchPromptNow awaits.
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-drain-3",
          seq: 2,
          event: { Stopped: { reason: "prompt_complete" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    expect(promptPostCount).toBe(1);

    // Mid-await: a new turn would normally have to start before the
    // user could enqueue, but the queue actions are independent. Push a
    // new turn (UserPromptSent) so turnActive flips back to true and a
    // sendPrompt enqueues rather than racing dispatchPromptNow.
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-drain-3",
          seq: 3,
          event: { UserPromptSent: { text: "echoed combined" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    act(() => {
      void result.current.sendPrompt("queued during await");
    });
    await flushAsync();
    expect(result.current.state.queuedPrompts).toHaveLength(3);

    // Resolve the in-flight POST as success; only the snapshot ids
    // should be removed -- the late entry stays.
    act(() => {
      resolvePost?.(new Response("{}", { status: 200 }));
    });
    await flushAsync();
    expect(result.current.state.queuedPrompts).toHaveLength(1);
    expect(result.current.state.queuedPrompts[0]?.text).toBe(
      "queued during await",
    );
  });
});

// Combined-mode drain splits the queue at clear-command boundaries
// (#1356). Without the split, queueing `/clear` between follow-ups got
// glued into one multi-paragraph POST and the server's head-anchored
// `is_clear_command` either misfired or missed the boundary entirely.
// Tests pump WS frames against a claude-profile session so the cockpit
// resolves `clearAliases = ["/clear"]`; each drain pass should now POST
// the leading sub-batch (either a standalone clear alias or the run of
// non-clear entries up to the next alias).

describe("useCockpit drain split at clear-command boundary (#1356)", () => {
  let promptPostCount: number;
  let promptPostBodies: string[];

  const claudeWrapper = ({ children }: { children: ReactNode }) =>
    createElement(AgentProfileProvider, { toolKey: "claude" }, children);

  beforeEach(() => {
    sockets.length = 0;
    promptPostCount = 0;
    promptPostBodies = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : input.toString();
        if (url.includes("/cockpit/replay")) {
          return new Response(
            JSON.stringify({ frames: [], lost: false, highest_seq: 0 }),
            { status: 200 },
          );
        }
        if (url.includes("/cockpit/prompt")) {
          promptPostCount += 1;
          if (typeof init?.body === "string") {
            promptPostBodies.push(init.body);
          }
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 200 });
      }),
    );
    originalWebSocket = global.WebSocket;
    global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    global.WebSocket = originalWebSocket;
    vi.unstubAllGlobals();
  });

  function bodyTexts(): string[] {
    return promptPostBodies.map((b) => {
      try {
        const parsed = JSON.parse(b) as { text?: string };
        return parsed.text ?? "";
      } catch {
        return b;
      }
    });
  }

  async function bootSession(sessionId: string) {
    const { result } = renderHook(() => useCockpit(sessionId), {
      wrapper: claudeWrapper,
    });
    await flushAsync();
    const ws = sockets[sockets.length - 1]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();
    let seq = 0;
    const nextSeq = () => {
      seq += 1;
      return seq;
    };
    // Kick a turn so subsequent sendPrompt calls enqueue.
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: sessionId,
          seq: nextSeq(),
          event: { UserPromptSent: { text: "kicker" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    async function pumpStopped() {
      act(() => {
        ws.onmessage?.({
          data: JSON.stringify({
            session_id: sessionId,
            seq: nextSeq(),
            event: { Stopped: { reason: "prompt_complete" } },
          }),
        } as MessageEvent);
      });
      await flushAsync();
    }
    return { result, ws, pumpStopped };
  }

  it("queue [a, /clear, b] fires three sub-batch POSTs in order", async () => {
    const { result, pumpStopped } = await bootSession("sess-split-1");
    act(() => {
      void result.current.sendPrompt("a");
    });
    act(() => {
      void result.current.sendPrompt("/clear");
    });
    act(() => {
      void result.current.sendPrompt("b");
    });
    await flushAsync();
    expect(result.current.state.queuedPrompts).toHaveLength(3);

    await pumpStopped();
    expect(promptPostCount).toBe(1);
    expect(bodyTexts()).toEqual(["a"]);
    expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual([
      "/clear",
      "b",
    ]);

    await pumpStopped();
    expect(promptPostCount).toBe(2);
    expect(bodyTexts()).toEqual(["a", "/clear"]);
    expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual([
      "b",
    ]);

    await pumpStopped();
    expect(promptPostCount).toBe(3);
    expect(bodyTexts()).toEqual(["a", "/clear", "b"]);
    expect(result.current.state.queuedPrompts).toEqual([]);
  });

  it("solo /clear in the queue fires standalone", async () => {
    const { result, pumpStopped } = await bootSession("sess-split-2");
    act(() => {
      void result.current.sendPrompt("/clear");
    });
    await flushAsync();
    expect(result.current.state.queuedPrompts).toHaveLength(1);

    await pumpStopped();
    expect(promptPostCount).toBe(1);
    expect(bodyTexts()).toEqual(["/clear"]);
    expect(result.current.state.queuedPrompts).toEqual([]);
  });

  it("queue [a, b, /clear] combines the leading non-clear prefix then fires /clear alone", async () => {
    const { result, pumpStopped } = await bootSession("sess-split-3");
    act(() => {
      void result.current.sendPrompt("a");
    });
    act(() => {
      void result.current.sendPrompt("b");
    });
    act(() => {
      void result.current.sendPrompt("/clear");
    });
    await flushAsync();
    expect(result.current.state.queuedPrompts).toHaveLength(3);

    await pumpStopped();
    expect(promptPostCount).toBe(1);
    expect(bodyTexts()).toEqual(["a\n\nb"]);
    expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual([
      "/clear",
    ]);

    await pumpStopped();
    expect(promptPostCount).toBe(2);
    expect(bodyTexts()).toEqual(["a\n\nb", "/clear"]);
    expect(result.current.state.queuedPrompts).toEqual([]);
  });

  it("queue [/clear, /clear, a] fires each /clear standalone before the trailing prompt", async () => {
    const { result, pumpStopped } = await bootSession("sess-split-4");
    act(() => {
      void result.current.sendPrompt("/clear");
    });
    act(() => {
      void result.current.sendPrompt("/clear");
    });
    act(() => {
      void result.current.sendPrompt("a");
    });
    await flushAsync();
    expect(result.current.state.queuedPrompts).toHaveLength(3);

    await pumpStopped();
    await pumpStopped();
    await pumpStopped();

    expect(promptPostCount).toBe(3);
    expect(bodyTexts()).toEqual(["/clear", "/clear", "a"]);
    expect(result.current.state.queuedPrompts).toEqual([]);
  });

  it("`/clear --hard` invocation is treated as a clear-command boundary", async () => {
    const { result, pumpStopped } = await bootSession("sess-split-5");
    act(() => {
      void result.current.sendPrompt("a");
    });
    act(() => {
      void result.current.sendPrompt("/clear --hard");
    });
    act(() => {
      void result.current.sendPrompt("b");
    });
    await flushAsync();

    await pumpStopped();
    await pumpStopped();
    await pumpStopped();

    expect(bodyTexts()).toEqual(["a", "/clear --hard", "b"]);
  });

  it("codex profile splits at `/new` boundaries", async () => {
    const codexWrapper = ({ children }: { children: ReactNode }) =>
      createElement(AgentProfileProvider, { toolKey: "codex" }, children);
    const { result: hookResult } = renderHook(
      () => useCockpit("sess-split-codex"),
      { wrapper: codexWrapper },
    );
    await flushAsync();
    const ws = sockets[sockets.length - 1]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();
    let seq = 0;
    const nextSeq = () => ++seq;
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-split-codex",
          seq: nextSeq(),
          event: { UserPromptSent: { text: "kicker" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    async function pumpStopped() {
      act(() => {
        ws.onmessage?.({
          data: JSON.stringify({
            session_id: "sess-split-codex",
            seq: nextSeq(),
            event: { Stopped: { reason: "prompt_complete" } },
          }),
        } as MessageEvent);
      });
      await flushAsync();
    }
    act(() => {
      void hookResult.current.sendPrompt("a");
    });
    act(() => {
      void hookResult.current.sendPrompt("/new");
    });
    act(() => {
      void hookResult.current.sendPrompt("b");
    });
    await flushAsync();

    await pumpStopped();
    await pumpStopped();
    await pumpStopped();

    expect(bodyTexts()).toEqual(["a", "/new", "b"]);
  });

  it("gemini profile (no clear aliases) keeps the original single-POST combined behavior", async () => {
    const geminiWrapper = ({ children }: { children: ReactNode }) =>
      createElement(AgentProfileProvider, { toolKey: "gemini" }, children);
    const { result: hookResult } = renderHook(
      () => useCockpit("sess-split-gemini"),
      { wrapper: geminiWrapper },
    );
    await flushAsync();
    const ws = sockets[sockets.length - 1]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();
    let seq = 0;
    const nextSeq = () => ++seq;
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-split-gemini",
          seq: nextSeq(),
          event: { UserPromptSent: { text: "kicker" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();
    act(() => {
      void hookResult.current.sendPrompt("a");
    });
    act(() => {
      void hookResult.current.sendPrompt("/clear");
    });
    act(() => {
      void hookResult.current.sendPrompt("b");
    });
    await flushAsync();

    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({
          session_id: "sess-split-gemini",
          seq: nextSeq(),
          event: { Stopped: { reason: "prompt_complete" } },
        }),
      } as MessageEvent);
    });
    await flushAsync();

    // Single POST with all three glued via blank-line join. `/clear` is
    // not a gemini clear-alias so no boundary fires.
    expect(promptPostCount).toBe(1);
    expect(bodyTexts()).toEqual(["a\n\n/clear\n\nb"]);
    expect(hookResult.current.state.queuedPrompts).toEqual([]);
  });
});

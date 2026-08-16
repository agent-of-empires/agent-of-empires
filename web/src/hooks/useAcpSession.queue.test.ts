// @vitest-environment jsdom
//
// Tests for the SERVER-owned prompt queue (see
// docs/development/server-side-prompt-queue.md). The daemon owns the queue and
// drains it; the client keeps an optimistic overlay, POSTs mutations to the
// /queue endpoints, and reconciles against the server snapshot via
// `hydrate_server_queue`. The old client-side drain (combined mode,
// clear-boundary split, background drain coordinator) is gone, so its tests
// are gone with it.

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { emptyAcpState } from "../lib/acpTypes";
import { reportAcpInteraction, type ServerQueuedPrompt } from "../lib/api";
import { acpHookReducer, clearAcpCache, useAcpSession } from "./useAcpSession";

// Spy on the telemetry ping while keeping the rest of the api module real
// (the hook also calls setSessionArchive / setSessionSnooze through it).
vi.mock("../lib/api", async (importActual) => {
  const actual = await importActual<typeof import("../lib/api")>();
  return { ...actual, reportAcpInteraction: vi.fn() };
});

describe("acpHookReducer / server-queue actions", () => {
  it("enqueue_prompt appends an optimistic pending row keyed by the caller's id", () => {
    const s1 = acpHookReducer(emptyAcpState(), { kind: "enqueue_prompt", id: "q1", text: "first" });
    const s2 = acpHookReducer(s1, { kind: "enqueue_prompt", id: "q2", text: "second" });
    expect(s2.queuedPrompts.map((q) => [q.id, q.text, q.pending])).toEqual([
      ["q1", "first", true],
      ["q2", "second", true],
    ]);
  });

  it("enqueue_prompt carries attachments and omits the key for a text-only send", () => {
    const withAtt = acpHookReducer(emptyAcpState(), {
      kind: "enqueue_prompt",
      id: "q1",
      text: "with image",
      attachments: [{ kind: "image", mimeType: "image/png", dataB64: "aA==", name: "x.png" }],
    });
    expect(withAtt.queuedPrompts[0]?.attachments?.[0]?.name).toBe("x.png");
    const textOnly = acpHookReducer(emptyAcpState(), { kind: "enqueue_prompt", id: "q1", text: "t", attachments: [] });
    expect(textOnly.queuedPrompts[0]?.attachments).toBeUndefined();
  });

  it("dequeue_prompt / edit_queued_prompt / clear_queue mutate the overlay", () => {
    let s = acpHookReducer(emptyAcpState(), { kind: "enqueue_prompt", id: "a", text: "first" });
    s = acpHookReducer(s, { kind: "enqueue_prompt", id: "b", text: "second" });
    s = acpHookReducer(s, { kind: "edit_queued_prompt", id: "b", text: "second (edited)" });
    expect(s.queuedPrompts[1]?.text).toBe("second (edited)");
    s = acpHookReducer(s, { kind: "dequeue_prompt", id: "a" });
    expect(s.queuedPrompts.map((q) => q.id)).toEqual(["b"]);
    s = acpHookReducer(s, { kind: "clear_queue" });
    expect(s.queuedPrompts).toEqual([]);
  });

  it("confirm_queued_prompt clears the pending flag on the matching row", () => {
    const s1 = acpHookReducer(emptyAcpState(), { kind: "enqueue_prompt", id: "q1", text: "x" });
    expect(s1.queuedPrompts[0]?.pending).toBe(true);
    const s2 = acpHookReducer(s1, { kind: "confirm_queued_prompt", id: "q1" });
    expect(s2.queuedPrompts[0]?.pending).toBe(false);
  });

  describe("hydrate_server_queue merge", () => {
    const serverRow = (
      id: string,
      seq: number,
      text: string,
      atts?: ServerQueuedPrompt["attachments"],
    ): ServerQueuedPrompt => ({
      id,
      seq,
      text,
      created_at: "2026-01-01T00:00:00.000Z",
      ...(atts ? { attachments: atts } : {}),
    });

    it("replaces the overlay with the server snapshot, dropping a confirmed local row the server no longer has", () => {
      const enqueued = acpHookReducer(emptyAcpState(), { kind: "enqueue_prompt", id: "old", text: "gone from server" });
      // Confirm it (pending cleared), so a later hydrate that omits it means
      // the server drained/removed it while we were away -> drop it locally.
      const confirmed = acpHookReducer(enqueued, { kind: "confirm_queued_prompt", id: "old" });
      const next = acpHookReducer(confirmed, {
        kind: "hydrate_server_queue",
        rows: [serverRow("a", 0, "one"), serverRow("b", 1, "two")],
      });
      expect(next.queuedPrompts.map((q) => q.text)).toEqual(["one", "two"]);
    });

    it("keeps a still-pending optimistic row the server snapshot does not have yet", () => {
      const local = acpHookReducer(emptyAcpState(), { kind: "enqueue_prompt", id: "inflight", text: "just typed" });
      const next = acpHookReducer(local, { kind: "hydrate_server_queue", rows: [serverRow("a", 0, "confirmed")] });
      expect(next.queuedPrompts.map((q) => q.text)).toEqual(["confirmed", "just typed"]);
    });

    it("keeps local attachment bytes for a row we queued, for the thumbnail", () => {
      const local = acpHookReducer(emptyAcpState(), {
        kind: "enqueue_prompt",
        id: "q1",
        text: "img",
        attachments: [{ kind: "image", mimeType: "image/png", dataB64: "REALBYTES", name: "a.png" }],
      });
      const confirmed = acpHookReducer(local, { kind: "confirm_queued_prompt", id: "q1" });
      const next = acpHookReducer(confirmed, {
        kind: "hydrate_server_queue",
        rows: [
          serverRow("q1", 0, "img", [{ id: "att1", kind: "image", mime_type: "image/png", name: "a.png", size: 9 }]),
        ],
      });
      // Local bytes win so the strip can still render the thumbnail.
      expect(next.queuedPrompts[0]?.attachments?.[0]?.dataB64).toBe("REALBYTES");
    });

    it("builds a metadata-only attachment view from server refs when we have no local bytes", () => {
      const next = acpHookReducer(emptyAcpState(), {
        kind: "hydrate_server_queue",
        rows: [
          serverRow("q1", 0, "reloaded", [
            { id: "att1", kind: "resource", mime_type: "text/plain", name: "notes.txt", size: 12 },
          ]),
        ],
      });
      const att = next.queuedPrompts[0]?.attachments?.[0];
      expect(att).toMatchObject({ kind: "resource", mimeType: "text/plain", name: "notes.txt", dataB64: "" });
    });
  });
});

// --- Hook integration: optimistic overlay + server POSTs ---

interface FakeSocket {
  url: string;
  readyState: number;
  onopen: ((ev: Event) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
  onerror: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  close: () => void;
  send: () => void;
}

const sockets: FakeSocket[] = [];
let originalWebSocket: typeof WebSocket;

class FakeWebSocket implements FakeSocket {
  url: string;
  readyState = 0;
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  constructor(url: string) {
    this.url = url;
    sockets.push(this);
  }
  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.({ code: 1000, reason: "test", wasClean: true } as CloseEvent);
  }
  send(): void {
    /* no-op */
  }
}

async function flushAsync(): Promise<void> {
  await act(async () => {
    for (let i = 0; i < 8; i++) await Promise.resolve();
  });
}

/** Records every fetch so tests can assert which endpoint was hit. Maintains a
 *  tiny in-memory server queue so GET /queue reflects prior POSTs (for the
 *  migration + hydrate paths). */
interface Recorded {
  method: string;
  url: string;
  body: string | null;
}

describe("useAcpSession server-queue integration", () => {
  let calls: Recorded[];
  let serverQueue: Map<string, ServerQueuedPrompt>;

  const queueCalls = (suffix: string) => calls.filter((c) => c.url.includes(suffix));

  beforeEach(() => {
    sockets.length = 0;
    calls = [];
    serverQueue = new Map();
    clearAcpCache();
    vi.mocked(reportAcpInteraction).mockClear();
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : input.toString();
        const method = (init?.method ?? "GET").toUpperCase();
        const body = typeof init?.body === "string" ? init.body : null;
        calls.push({ method, url, body });
        if (url.includes("/acp/replay")) {
          return new Response(JSON.stringify({ frames: [], lost: false, highest_seq: 0 }), { status: 200 });
        }
        // /queue endpoints. Order matters: check the item path first.
        const queueItem = url.match(/\/queue\/([^/?]+)/);
        if (url.includes("/queue")) {
          if (method === "GET") {
            const rows = [...serverQueue.values()].sort((a, b) => a.seq - b.seq);
            return new Response(JSON.stringify(rows), { status: 200 });
          }
          if (method === "POST" && body) {
            const parsed = JSON.parse(body) as { id: string; text: string; created_at?: string };
            const row: ServerQueuedPrompt = {
              id: parsed.id,
              seq: serverQueue.size,
              text: parsed.text,
              created_at: parsed.created_at ?? "2026-01-01T00:00:00.000Z",
            };
            serverQueue.set(parsed.id, row);
            return new Response(JSON.stringify(row), { status: 200 });
          }
          if (method === "DELETE") {
            if (queueItem) serverQueue.delete(decodeURIComponent(queueItem[1]!));
            else serverQueue.clear();
            return new Response(null, { status: 204 });
          }
          if (method === "PATCH") return new Response(null, { status: 204 });
        }
        if (url.includes("/acp/prompt")) return new Response("{}", { status: 200 });
        if (url.includes("/acp/cancel")) return new Response("{}", { status: 200 });
        return new Response("{}", { status: 200 });
      }),
    );
    originalWebSocket = global.WebSocket;
    global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    global.WebSocket = originalWebSocket;
    vi.unstubAllGlobals();
    clearAcpCache();
  });

  async function openSession(id: string) {
    const hook = renderHook(() => useAcpSession(id));
    await flushAsync();
    const ws = sockets[sockets.length - 1]!;
    act(() => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.({} as Event);
    });
    await flushAsync();
    return { ...hook, ws };
  }

  it("enqueues optimistically and POSTs to the server queue when a turn is busy", async () => {
    const { result, ws } = await openSession("sess-busy");
    // Kick a turn so turnActive flips on; a follow-up must queue.
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({ session_id: "sess-busy", seq: 1, event: { UserPromptSent: { text: "kick" } } }),
      } as MessageEvent);
    });
    await flushAsync();
    expect(result.current.state.turnActive).toBe(true);

    act(() => {
      void result.current.sendPrompt("follow-up");
    });
    await flushAsync();

    // Optimistic row shows immediately, then the enqueue POST confirms it. No
    // direct /acp/prompt fires (the server drains it).
    expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["follow-up"]);
    expect(result.current.state.queuedPrompts[0]?.pending).toBe(false);
    const posts = queueCalls("/queue").filter((c) => c.method === "POST");
    expect(posts).toHaveLength(1);
    expect(JSON.parse(posts[0]!.body!)).toMatchObject({ text: "follow-up" });
    expect(queueCalls("/acp/prompt").filter((c) => c.method === "POST")).toHaveLength(0);
    expect(reportAcpInteraction).toHaveBeenCalledWith("prompt_queued");
  });

  it("POSTs directly (no queue) when the session is idle and open", async () => {
    const { result } = await openSession("sess-idle");
    await act(async () => {
      await result.current.sendPrompt("send now");
    });
    await flushAsync();
    expect(queueCalls("/acp/prompt").filter((c) => c.method === "POST")).toHaveLength(1);
    expect(queueCalls("/queue").filter((c) => c.method === "POST")).toHaveLength(0);
  });

  it("remove / edit / clear mirror to the server endpoints", async () => {
    const { result, ws } = await openSession("sess-mut");
    act(() => {
      ws.onmessage?.({
        data: JSON.stringify({ session_id: "sess-mut", seq: 1, event: { UserPromptSent: { text: "kick" } } }),
      } as MessageEvent);
    });
    await flushAsync();
    act(() => {
      void result.current.sendPrompt("row");
    });
    await flushAsync();
    const id = result.current.state.queuedPrompts[0]!.id;

    act(() => {
      result.current.editQueuedPrompt(id, "row edited");
    });
    act(() => {
      result.current.removeQueuedPrompt(id);
    });
    await flushAsync();
    expect(queueCalls(`/queue/${encodeURIComponent(id)}`).map((c) => c.method)).toEqual(["PATCH", "DELETE"]);
    expect(result.current.state.queuedPrompts).toEqual([]);

    act(() => {
      result.current.clearQueue();
    });
    await flushAsync();
    // A whole-queue clear is DELETE /queue (no id segment).
    expect(calls.some((c) => c.method === "DELETE" && /\/queue$/.test(c.url.split("?")[0]!))).toBe(true);
  });

  it("migrates local rows to the server and hydrates from the snapshot on connect", async () => {
    // Pre-seed a local optimistic row (as a reload would restore), then connect.
    // The migration POSTs it to the server; the hydrate list then reflects it.
    serverQueue.set("pre", { id: "pre", seq: 0, text: "migrated", created_at: "2026-01-01T00:00:00.000Z" });
    const { result } = await openSession("sess-migrate");
    await flushAsync();
    // GET /queue ran on connect and hydrated the row.
    expect(queueCalls("/queue").some((c) => c.method === "GET")).toBe(true);
    expect(result.current.state.queuedPrompts.map((q) => q.text)).toEqual(["migrated"]);
  });
});

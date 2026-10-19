import { afterEach, describe, expect, it, vi } from "vitest";

import { emptyAcpState, type ActivityRow, type TranscriptRow } from "../../lib/acpTypes";
import { reducer } from "./reducer";
import { fetchReplay } from "./replay";

type Page = { rows: TranscriptRow[]; next_cursor: number | null; has_more: boolean; highest_seq?: number };

const row = (id: string, kind: TranscriptRow["kind"]): TranscriptRow => ({
  id,
  group_id: id,
  kind,
  at: "2026-01-01T00:00:00Z",
  text: id,
});

// `pages` are keyed by the `before` cursor they answer; the tail answers `before=MAX_SAFE_INTEGER`.
// `since` pages answer `since=N` by cursor; the rows view carries `rows`, the frames view none.
function mockReplay(pages: Record<string, Page>, since: Record<string, TranscriptRow[]> = {}) {
  const requests: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      requests.push(url);
      const params = new URLSearchParams(url.split("?")[1]);
      if (params.has("since")) {
        const rows = params.get("view") === "rows" ? (since[params.get("since")!] ?? []) : null;
        return new Response(JSON.stringify({ frames: [], rows, lost: false, highest_seq: 3000 }));
      }
      const page = pages[params.get("before") ?? ""] ?? { rows: [], next_cursor: null, has_more: false };
      const body = params.has("before")
        ? { frames: [], rows: params.get("view") === "rows" ? page.rows : null, lost: false, highest_seq: 100, ...page }
        : { frames: [], lost: false, highest_seq: 100 };
      return new Response(JSON.stringify(body));
    }),
  );
  return requests;
}

async function reopen(signal = new AbortController().signal, lastSeq = 0, oldestSeq = 0) {
  const actions: { kind: string; frames?: unknown[]; rows?: ActivityRow[]; oldestSeq?: number }[] = [];
  const handshakes = () => actions.filter((a) => a.kind === "handshake");
  const setHasMore = vi.fn();
  const cursor = { current: lastSeq };
  await fetchReplay("sid", cursor, oldestSeq, (a) => actions.push(a as (typeof actions)[number]), setHasMore, signal);
  return { frames: actions.filter((a) => a.kind === "frames"), setHasMore, handshakes, cursor };
}
const coldOpen = (signal?: AbortSignal) => reopen(signal);

const TAIL = String(Number.MAX_SAFE_INTEGER);

afterEach(() => vi.unstubAllGlobals());

describe("rows recovery for a cached cursor without history", () => {
  it("loads the transcript from the tail, then folds frames forward from the cached cursor", async () => {
    const requests = mockReplay({
      [TAIL]: { rows: [row("t1", "tool_start")], next_cursor: 80, has_more: true },
      "80": { rows: [row("u", "user_prompt")], next_cursor: 40, has_more: true },
    });
    const { frames, cursor, handshakes } = await reopen(undefined, 900);
    expect(frames[0]).toMatchObject({ frames: [], oldestSeq: 40 });
    expect(frames[0]!.rows!.map((r) => r.id)).toEqual(["u", "t1"]);
    expect(cursor.current).toBe(900);
    expect(handshakes()).toEqual([]);
    expect(requests.some((u) => u.includes("since=850&"))).toBe(true);
    expect(requests.findIndex((u) => u.includes("before="))).toBeLessThan(
      requests.findIndex((u) => u.includes("since=")),
    );
  });

  it("keeps older forward rows out of the recovered transcript, but takes rows newer than the tail", async () => {
    // The tail ends at seq 3000 (highest_seq); frames from the cached cursor 900 would replay older turns.
    const requests = mockReplay(
      {
        [TAIL]: {
          rows: [row("u2", "user_prompt"), row("t2", "tool_start")],
          next_cursor: 2100,
          has_more: true,
          highest_seq: 3000,
        },
      },
      {
        "850": [row("u1", "user_prompt"), row("t1", "tool_start")],
        "2950": [row("t2", "tool_start"), row("new", "agent_message")],
      },
    );
    const actions: Parameters<typeof reducer>[1][] = [];
    const cursor = { current: 900 };
    await fetchReplay("sid", cursor, 0, (a) => actions.push(a), vi.fn(), new AbortController().signal);
    const state = actions.reduce(reducer, { ...emptyAcpState(), lastSeq: 900 });
    expect(state.activity.map((r) => r.id)).toEqual(["u2", "t2", "new"]);
    expect(requests.some((u) => u.includes("since=850") && u.includes("view=rows"))).toBe(false);
    expect(requests.some((u) => u.includes("since=2950") && u.includes("view=rows"))).toBe(true);
  });

  it("only folds forward when history is already loaded", async () => {
    const requests = mockReplay({});
    await reopen(undefined, 900, 40);
    expect(requests.some((u) => u.includes("before="))).toBe(false);
    expect(requests.some((u) => u.includes("since=850&"))).toBe(true);
  });
});

describe("fetchTail", () => {
  it("keeps paging older until the page holds a user prompt, then dispatches once", async () => {
    mockReplay({
      [TAIL]: { rows: [row("t1", "tool_start")], next_cursor: 80, has_more: true },
      "80": { rows: [row("t0", "tool_start")], next_cursor: 60, has_more: true },
      "60": { rows: [row("u", "user_prompt"), row("a", "agent_message")], next_cursor: 40, has_more: true },
    });
    const { frames, setHasMore } = await coldOpen();
    expect(frames).toHaveLength(1);
    expect(frames[0]!.rows!.map((r) => r.id)).toEqual(["u", "a", "t0", "t1"]);
    expect(frames[0]!.oldestSeq).toBe(40);
    expect(setHasMore).toHaveBeenCalledWith(true);
  });

  it("does not page older when the tail already holds a user prompt", async () => {
    const requests = mockReplay({
      [TAIL]: { rows: [row("u", "user_prompt"), row("t", "tool_start")], next_cursor: 80, has_more: true },
    });
    const { frames } = await coldOpen();
    expect(frames[0]!.rows!.map((r) => r.id)).toEqual(["u", "t"]);
    expect(frames[0]!.oldestSeq).toBe(80);
    expect(requests.filter((u) => u.includes("before=80"))).toEqual([]);
  });

  it("stops at the start of history when no prompt exists", async () => {
    mockReplay({
      [TAIL]: { rows: [row("t1", "tool_start")], next_cursor: 80, has_more: true },
      "80": { rows: [row("t0", "tool_start")], next_cursor: 1, has_more: false },
    });
    const { frames, setHasMore } = await coldOpen();
    expect(frames[0]!.rows!.map((r) => r.id)).toEqual(["t0", "t1"]);
    expect(setHasMore).toHaveBeenCalledWith(false);
  });

  it("merges a tool start split across the page seam into the tail placeholder", async () => {
    const toolStart = (id: string, name: string, startedAt: string): TranscriptRow => ({
      ...row(id, "tool_start"),
      tool_call_id: "c1",
      tool: { id: "c1", name, kind: "read", args_preview: "", started_at: startedAt },
    });
    mockReplay({
      [TAIL]: {
        rows: [toolStart("start-c1", "tool call", "2026-01-01T00:05:00Z")],
        next_cursor: 80,
        has_more: true,
      },
      "80": {
        rows: [row("u", "user_prompt"), toolStart("tool-c1", "Read", "2026-01-01T00:00:00Z")],
        next_cursor: 40,
        has_more: true,
      },
    });
    const { frames } = await coldOpen();
    const starts = frames[0]!.rows!.filter((r) => r.kind === "tool_start");
    expect(starts).toHaveLength(1);
    expect(starts[0]!.tool).toMatchObject({ name: "Read", started_at: "2026-01-01T00:00:00Z" });
  });

  it("keeps the tail when an older page fetch throws", async () => {
    mockReplay({ [TAIL]: { rows: [row("t1", "tool_start")], next_cursor: 80, has_more: true } });
    const fetchMock = globalThis.fetch as unknown as ReturnType<typeof vi.fn>;
    const impl = fetchMock.getMockImplementation()!;
    fetchMock.mockImplementation(async (url: string) => {
      if (url.includes("before=80")) throw new TypeError("network down");
      return impl(url);
    });
    const { frames, setHasMore } = await coldOpen();
    expect(frames).toHaveLength(1);
    expect(frames[0]!.rows!.map((r) => r.id)).toEqual(["t1"]);
    expect(frames[0]!.oldestSeq).toBe(80);
    expect(setHasMore).toHaveBeenCalledWith(true);
  });

  it("stops after the page cap and leaves older history for loadOlder", async () => {
    const pages: Record<string, Page> = {
      [TAIL]: { rows: [row("t-tail", "tool_start")], next_cursor: 700, has_more: true },
    };
    for (let c = 700; c > 100; c -= 100) {
      pages[String(c)] = { rows: [row(`t${c}`, "tool_start")], next_cursor: c - 100, has_more: true };
    }
    const requests = mockReplay(pages);
    const { frames, setHasMore } = await coldOpen();
    expect(requests.filter((u) => /before=\d+&limit=\d+&view=rows/.test(u) && !u.includes(TAIL))).toHaveLength(5);
    expect(frames).toHaveLength(1);
    expect(frames[0]!.oldestSeq).toBe(200);
    expect(setHasMore).toHaveBeenCalledWith(true);
  });

  it("still fetches the handshake when paging reaches the start of history", async () => {
    const requests = mockReplay({
      [TAIL]: { rows: [row("t1", "tool_start")], next_cursor: 80, has_more: true },
      "80": { rows: [row("u", "user_prompt")], next_cursor: 1, has_more: false },
    });
    const fetchMock = globalThis.fetch as unknown as ReturnType<typeof vi.fn>;
    const impl = fetchMock.getMockImplementation()!;
    fetchMock.mockImplementation(async (url: string) => {
      if (!url.includes("since=0")) return impl(url);
      requests.push(url);
      return new Response(JSON.stringify({ frames: [{ seq: 1 }], lost: false, highest_seq: 100 }));
    });
    const { handshakes } = await coldOpen();
    expect(requests.some((u) => u.includes("since=0"))).toBe(true);
    expect(handshakes()).toHaveLength(1);
  });

  it("dispatches nothing when aborted mid-paging", async () => {
    const abort = new AbortController();
    mockReplay({
      [TAIL]: { rows: [row("t1", "tool_start")], next_cursor: 80, has_more: true },
      "80": { rows: [row("u", "user_prompt")], next_cursor: 60, has_more: true },
    });
    const fetchMock = globalThis.fetch as unknown as ReturnType<typeof vi.fn>;
    const impl = fetchMock.getMockImplementation()!;
    fetchMock.mockImplementation(async (url: string) => {
      if (url.includes("before=80")) abort.abort();
      return impl(url);
    });
    const { frames } = await coldOpen(abort.signal);
    expect(frames).toEqual([]);
  });
});

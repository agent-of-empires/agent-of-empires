import { afterEach, describe, expect, it, vi } from "vitest";

import type { ActivityRow, TranscriptRow } from "../../lib/acpTypes";
import { fetchReplay } from "./replay";

type Page = { rows: TranscriptRow[]; next_cursor: number | null; has_more: boolean };

const row = (id: string, kind: TranscriptRow["kind"]): TranscriptRow => ({
  id,
  group_id: id,
  kind,
  at: "2026-01-01T00:00:00Z",
  text: id,
});

// `pages` are keyed by the `before` cursor they answer; the tail answers `before=MAX_SAFE_INTEGER`.
function mockReplay(pages: Record<string, Page>) {
  const requests: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      requests.push(url);
      const params = new URLSearchParams(url.split("?")[1]);
      const page = pages[params.get("before") ?? ""] ?? { rows: [], next_cursor: null, has_more: false };
      const body = params.has("before")
        ? { frames: [], rows: params.get("view") === "rows" ? page.rows : null, lost: false, highest_seq: 100, ...page }
        : { frames: [], lost: false, highest_seq: 100 };
      return new Response(JSON.stringify(body));
    }),
  );
  return requests;
}

async function coldOpen(signal = new AbortController().signal) {
  const actions: { kind: string; rows?: ActivityRow[]; oldestSeq?: number }[] = [];
  const setHasMore = vi.fn();
  await fetchReplay("sid", { current: 0 }, (a) => actions.push(a as (typeof actions)[number]), setHasMore, signal);
  return { frames: actions.filter((a) => a.kind === "frames"), setHasMore };
}

const TAIL = String(Number.MAX_SAFE_INTEGER);

afterEach(() => vi.unstubAllGlobals());

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

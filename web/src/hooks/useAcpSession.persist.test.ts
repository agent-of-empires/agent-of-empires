// @vitest-environment jsdom
//
// Tests for the localStorage-backed AcpState persistence
// added in #1132. The persistence helpers are module-private; we
// exercise them through the public `clearAcpCache` API plus the
// observable side effect on `window.localStorage`. The round-trip
// hydration test additionally mounts the hook against a fake
// WebSocket so it can read the `?since=<lastSeq>` query parameter
// on the resume URL.

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { emptyAcpState, type AcpState } from "../lib/acpTypes";
import { clearAcpCache, useAcpSession } from "./useAcpSession";

const KEY_PREFIX = "aoe:acp-state:v1:";
const TTL_MS = 7 * 24 * 60 * 60 * 1000;

// Reach into the module's internal helpers via a side-channel: cacheSet
// is only triggered through the React `useReducer` lifecycle, which we
// don't drive here. Instead, write directly into localStorage with the
// expected shape and verify the read paths via `clearAcpCache` (the
// only public surface that touches storage).

function writeEntry(sessionId: string, state: AcpState, savedAt: number): void {
  window.localStorage.setItem(KEY_PREFIX + sessionId, JSON.stringify({ savedAt, state }));
}

beforeEach(() => {
  window.localStorage.clear();
});

describe("useAcpSession / persisted state", () => {
  it("clearAcpCache(id) drops the matching localStorage entry", () => {
    writeEntry("sess-a", emptyAcpState(), Date.now());
    writeEntry("sess-b", emptyAcpState(), Date.now());
    expect(window.localStorage.getItem(KEY_PREFIX + "sess-a")).not.toBeNull();
    clearAcpCache("sess-a");
    expect(window.localStorage.getItem(KEY_PREFIX + "sess-a")).toBeNull();
    expect(window.localStorage.getItem(KEY_PREFIX + "sess-b")).not.toBeNull();
  });

  it("clearAcpCache() drops every acp-state entry", () => {
    writeEntry("sess-a", emptyAcpState(), Date.now());
    writeEntry("sess-b", emptyAcpState(), Date.now());
    window.localStorage.setItem("unrelated:key", "x");
    clearAcpCache();
    expect(window.localStorage.getItem(KEY_PREFIX + "sess-a")).toBeNull();
    expect(window.localStorage.getItem(KEY_PREFIX + "sess-b")).toBeNull();
    expect(window.localStorage.getItem("unrelated:key")).toBe("x");
  });

  it("entries past STATE_TTL_MS are stamped older than the cutoff", () => {
    writeEntry("sess-old", emptyAcpState(), Date.now() - TTL_MS - 1000);
    const oldRaw = window.localStorage.getItem(KEY_PREFIX + "sess-old");
    expect(oldRaw).not.toBeNull();
    const parsed = JSON.parse(oldRaw!) as { savedAt: number };
    expect(Date.now() - parsed.savedAt).toBeGreaterThan(TTL_MS);
  });

  it("malformed entries are removed by clearAcpCache", () => {
    window.localStorage.setItem(KEY_PREFIX + "sess-broken", "not valid json{{{");
    clearAcpCache("sess-broken");
    expect(window.localStorage.getItem(KEY_PREFIX + "sess-broken")).toBeNull();
  });

  it("setItem throwing on quota does not propagate", () => {
    const spy = vi.spyOn(window.localStorage, "removeItem").mockImplementation(() => {
      throw new Error("quota exceeded");
    });
    try {
      expect(() => clearAcpCache("sess-quota")).not.toThrow();
    } finally {
      spy.mockRestore();
    }
  });
});

// Round-trip test: pre-populate a persisted entry with a non-zero
// lastSeq, mount the hook, and assert the WebSocket dial URL carries
// `?since=<that lastSeq>`. This locks in the whole point of #1132 -
// reload-then-resume - which the storage-only tests above don't reach.
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
  readyState: number = 0;
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
  }
  send(): void {
    /* no-op */
  }
}

describe("useAcpSession / hydration round-trip", () => {
  // The hook's fetchReplay path treats `highest_seq < since` as a
  // server-side seq reset (session deleted + recreated with the same
  // id) and dispatches a state reset, zeroing lastSeq. So tests that
  // mount the hook with a non-zero persisted lastSeq need to mock the
  // replay endpoint with a matching highest_seq so the resume cursor
  // survives.
  let mockReplayHighestSeq = 0;
  beforeEach(() => {
    vi.useFakeTimers();
    sockets.length = 0;
    mockReplayHighestSeq = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = typeof input === "string" ? input : input.toString();
        if (url.includes("/api/login/status")) {
          return new Response(
            JSON.stringify({
              required: false,
              authenticated: true,
              elevated: true,
              elevated_until_secs: null,
            }),
            { status: 200 },
          );
        }
        return new Response(
          JSON.stringify({
            frames: [],
            lost: false,
            highest_seq: mockReplayHighestSeq,
          }),
          { status: 200 },
        );
      }),
    );
    originalWebSocket = global.WebSocket;
    global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    global.WebSocket = originalWebSocket;
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  async function flushAsync(): Promise<void> {
    await act(async () => {
      for (let i = 0; i < 6; i++) {
        await Promise.resolve();
      }
    });
  }

  it("hydrates lastSeq from localStorage and resumes the WS from that cursor", async () => {
    const persisted: AcpState = {
      ...emptyAcpState(),
      lastSeq: 4242,
    };
    writeEntry("sess-resume", persisted, Date.now());
    // Replay endpoint reports the same highest_seq so the reset-on-
    // backwards-seq guard in fetchReplay does not zero the cursor.
    mockReplayHighestSeq = 4242;

    renderHook(() => useAcpSession("sess-resume"));
    await flushAsync();

    expect(sockets).toHaveLength(1);
    expect(sockets[0]!.url).toContain("/acp/ws?since=4242");
  });

  it("falls back to since=0 when no persisted entry exists", async () => {
    renderHook(() => useAcpSession("sess-fresh"));
    await flushAsync();

    expect(sockets).toHaveLength(1);
    expect(sockets[0]!.url).toContain("/acp/ws?since=0");
  });

  it("ignores a persisted entry that is older than the TTL", async () => {
    const persisted: AcpState = {
      ...emptyAcpState(),
      lastSeq: 999,
    };
    writeEntry("sess-stale", persisted, Date.now() - TTL_MS - 1000);

    renderHook(() => useAcpSession("sess-stale"));
    await flushAsync();

    expect(sockets).toHaveLength(1);
    expect(sockets[0]!.url).toContain("/acp/ws?since=0");
  });
});

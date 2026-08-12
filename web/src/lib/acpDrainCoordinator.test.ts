// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  BACKGROUND_DRAIN_MAX_AGE_MS,
  __resetDrainCoordinator,
  armForBackgroundDrain,
  emitQueueRetired,
  isArmedForBackgroundDrain,
  isDraining,
  onAnyQueueRetired,
  onQueueRetired,
  withDrainLock,
} from "./acpDrainCoordinator";
import { STORAGE_KEY_PREFIX } from "./acpStateStorage";

/** Write a persisted structured-view entry whose newest queued row is
 *  `ageMs` old, matching what `persistState` stores. */
function seedPersistedQueue(sessionId: string, ageMs: number): void {
  window.localStorage.setItem(
    STORAGE_KEY_PREFIX + sessionId,
    JSON.stringify({
      savedAt: Date.now(),
      state: {
        lastSeq: 1,
        activity: [],
        queuedPrompts: [{ id: "q1", text: "hi", queuedAt: new Date(Date.now() - ageMs).toISOString() }],
      },
    }),
  );
}

/** Deferred promise, so a test can hold a drain open across assertions. */
function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

beforeEach(() => {
  window.localStorage.clear();
  __resetDrainCoordinator();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("withDrainLock", () => {
  it("admits one owner per session and releases afterwards", async () => {
    const gate = deferred();
    const calls: string[] = [];

    const first = withDrainLock("s1", async () => {
      calls.push("first");
      await gate.promise;
    });
    expect(isDraining("s1")).toBe(true);

    // Second owner for the SAME session while the first is in flight:
    // skipped entirely rather than queued. Queueing would re-POST rows the
    // first owner has already sent by the time the lock frees.
    await withDrainLock("s1", async () => {
      calls.push("second");
    });
    expect(calls).toEqual(["first"]);

    // A different session is unaffected; the lock is per session, not global.
    await withDrainLock("s2", async () => {
      calls.push("other-session");
    });
    expect(calls).toEqual(["first", "other-session"]);

    gate.resolve();
    await first;
    expect(isDraining("s1")).toBe(false);

    // Released, so a later drain for the same session runs.
    await withDrainLock("s1", async () => {
      calls.push("later");
    });
    expect(calls).toEqual(["first", "other-session", "later"]);
  });

  it("releases the lock when the drain throws, and surfaces the error", async () => {
    await expect(
      withDrainLock("s1", async () => {
        throw new Error("post failed");
      }),
    ).rejects.toThrow("post failed");
    expect(isDraining("s1")).toBe(false);
  });

  it("skips the drain when another tab holds the Web Lock", async () => {
    const ran: string[] = [];
    // jsdom has no navigator.locks, so the cross-tab branch needs a stub.
    // `ifAvailable` hands the callback a null lock when another tab owns it.
    const request = vi.fn(
      async (_name: string, _opts: unknown, cb: (lock: unknown) => Promise<void>) => await cb(null),
    );
    vi.stubGlobal("navigator", { ...window.navigator, locks: { request } });

    await withDrainLock("s1", async () => {
      ran.push("drained");
    });
    expect(ran).toEqual([]);
    expect(request).toHaveBeenCalledWith(
      "aoe:acp-drain:s1",
      { mode: "exclusive", ifAvailable: true },
      expect.any(Function),
    );
    expect(isDraining("s1")).toBe(false);
  });
});

describe("queue retirement broadcast", () => {
  it("fans delivered ids out to every listener for that session only", () => {
    const seen: string[][] = [];
    const otherSeen: string[][] = [];
    const off = onQueueRetired("s1", (ids) => seen.push(ids));
    onQueueRetired("s1", (ids) => seen.push(ids));
    onQueueRetired("s2", (ids) => otherSeen.push(ids));

    emitQueueRetired("s1", ["q1", "q2"]);
    expect(seen).toEqual([
      ["q1", "q2"],
      ["q1", "q2"],
    ]);
    expect(otherSeen).toEqual([]);

    // An empty retirement is never broadcast: it would re-run every drain
    // effect and turn a retryable failure into a hot retry loop.
    emitQueueRetired("s1", []);
    expect(seen).toHaveLength(2);

    off();
    emitQueueRetired("s1", ["q3"]);
    expect(seen).toEqual([["q1", "q2"], ["q1", "q2"], ["q3"]]);
  });
});

describe("cross-tab queue retirement", () => {
  const CHANNEL = "aoe:acp-queue-retired";

  // BroadcastChannel delivery is a queued task with no bound on when it
  // runs, so every assertion below is polled with `vi.waitFor` rather than
  // read after a fixed tick. A `setTimeout(0)` barrier passes locally and
  // fails on a loaded CI runner.

  it("publishes local retirements to other tabs", async () => {
    const other = new BroadcastChannel(CHANNEL);
    const received: unknown[] = [];
    other.onmessage = (ev) => received.push(ev.data);
    try {
      emitQueueRetired("s1", ["q1"]);
      await vi.waitFor(() => {
        expect(received).toEqual([{ sessionId: "s1", ids: ["q1"] }]);
      });
    } finally {
      other.close();
    }
  });

  it("applies a retirement announced by another tab", async () => {
    // The drain lock stops two tabs sending at once, but the loser still
    // holds the delivered rows in its own reducer and cache, so it would
    // re-send them once the lock frees. The announcement is what drops
    // them there.
    const anySeen: [string, string[]][] = [];
    const sessionSeen: string[][] = [];
    onAnyQueueRetired((sessionId, ids) => anySeen.push([sessionId, ids]));
    onQueueRetired("s1", (ids) => sessionSeen.push(ids));

    const other = new BroadcastChannel(CHANNEL);
    try {
      other.postMessage({ sessionId: "s1", ids: ["q1", "q2"] });
      await vi.waitFor(() => {
        expect(sessionSeen).toEqual([["q1", "q2"]]);
      });
      expect(anySeen).toEqual([["s1", ["q1", "q2"]]]);

      // Malformed and empty announcements are dropped rather than fanned
      // out; an empty retirement would re-run every drain effect. Asserted
      // against a following good message rather than a timeout: a channel
      // preserves order, so q3 arriving without the two bad ones adding
      // entries proves they were ignored.
      other.postMessage({ sessionId: "s1", ids: [] });
      other.postMessage({ nonsense: true });
      other.postMessage({ sessionId: "s1", ids: ["q3"] });
      await vi.waitFor(() => {
        expect(sessionSeen).toEqual([["q1", "q2"], ["q3"]]);
      });
    } finally {
      other.close();
    }
  });
});

describe("background drain arming", () => {
  it("arms a queue parked this page load, and a restored one only while fresh", () => {
    // Nothing known about this session at all.
    expect(isArmedForBackgroundDrain("s-unknown")).toBe(false);

    // Parked in this page's lifetime: unambiguously still wanted.
    armForBackgroundDrain("s-live");
    expect(isArmedForBackgroundDrain("s-live")).toBe(true);

    seedPersistedQueue("s-fresh", BACKGROUND_DRAIN_MAX_AGE_MS / 2);
    expect(isArmedForBackgroundDrain("s-fresh")).toBe(true);

    // Older than the window: still queued and still badged, but it waits
    // for the user to open that chat instead of firing at an agent they
    // have moved on from. Persisted state lives 7 days, far too long to
    // double as authorization to send.
    seedPersistedQueue("s-stale", BACKGROUND_DRAIN_MAX_AGE_MS * 2);
    expect(isArmedForBackgroundDrain("s-stale")).toBe(false);

    // A stale verdict is memoised, so a later enqueue is what re-arms it,
    // not a re-read of storage.
    seedPersistedQueue("s-stale", 0);
    expect(isArmedForBackgroundDrain("s-stale")).toBe(false);
    armForBackgroundDrain("s-stale");
    expect(isArmedForBackgroundDrain("s-stale")).toBe(true);
  });
});

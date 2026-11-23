// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  applyEvent,
  emptyAcpState,
  type AcpEvent,
  type AcpState,
  type ActivityRow,
  type BackgroundAgent,
  type BackgroundShell,
} from "../../lib/acpTypes";
import { PENDING_DONE_KEY_PREFIX, PENDING_KEY_PREFIX, STORAGE_KEY_PREFIX } from "../../lib/acpStateStorage";
import {
  cacheGet,
  cacheSet,
  clearAcpCache,
  evictOldestPersistedAcpState,
  flushPersist,
  loadPersistedState,
  persistState,
  useBackgroundAgents,
  useBackgroundShells,
} from "./stateCache";

const key = (id: string) => STORAGE_KEY_PREFIX + id;
const CURRENT = key("sess-current");
const DAY_MS = 86_400_000;

function writeEntry(id: string, state: Record<string, unknown> = { ...emptyAcpState() }, savedAt = Date.now()) {
  localStorage.setItem(key(id), JSON.stringify({ savedAt, state }));
}

const quotaError = () => new DOMException("The quota has been exceeded.", "QuotaExceededError");

beforeEach(() => localStorage.clear());
afterEach(() => vi.restoreAllMocks());

describe("loadPersistedState", () => {
  it.each(["pendingElicitations", "oldestSeq", "startupCause"])("backfills %s missing from an older entry", (field) => {
    const legacy: Record<string, unknown> = { ...emptyAcpState() };
    delete legacy[field];
    writeEntry("legacy", legacy);
    expect(loadPersistedState("legacy")).toMatchObject({
      [field]: (emptyAcpState() as unknown as Record<string, unknown>)[field],
    });
  });

  it("preserves stored values", () => {
    const elicitation = { nonce: "e-1", message: "Pick", questions: [], resolved: null };
    const activity = [{ id: "u", kind: "user_prompt", text: "u", at: "t" }];
    writeEntry("seq", { ...emptyAcpState(), oldestSeq: 42, activity, pendingElicitations: [elicitation] });
    const loaded = loadPersistedState("seq");
    expect(loaded?.oldestSeq).toBe(42);
    expect(loaded?.pendingElicitations.map((e) => e.nonce)).toEqual(["e-1"]);
  });
});

describe("loadPersistedState mid-turn cache (#4530)", () => {
  const act = (id: string, kind: ActivityRow["kind"]): ActivityRow => ({ id, kind, text: id, at: "t" }) as ActivityRow;
  const queued = [{ id: "q1", text: "next", queuedAt: "2026-01-01T00:00:00Z" }];
  const configOptions = [
    { id: "model", name: "Model", category: "model", current_value: "opus", options: [] },
  ] as AcpState["configOptions"];
  const backgroundAgents = [{ agentId: "a1", toolCallId: "tc1", status: "running" }] as AcpState["backgroundAgents"];
  const entry = (activity: ActivityRow[], oldestSeq: number) => ({
    ...emptyAcpState(),
    lastSeq: 900,
    oldestSeq,
    activity,
    queuedPrompts: queued,
    configOptions,
    backgroundAgents,
  });

  it.each([
    ["no prompt, older history pending", [act("t", "tool_start")], 40, false],
    ["prompt loaded", [act("u", "user_prompt"), act("t", "tool_start")], 40, true],
    ["no prompt, whole history loaded", [act("t", "tool_start")], 1, true],
  ])("%s", (_name, activity, oldestSeq, kept) => {
    writeEntry("mid", entry(activity, oldestSeq));
    const loaded = loadPersistedState("mid")!;
    expect(loaded.queuedPrompts).toEqual(queued);
    expect(loaded.configOptions).toEqual(configOptions);
    expect(loaded.backgroundAgents).toEqual(backgroundAgents);
    if (kept) expect(loaded).toMatchObject({ lastSeq: 900, oldestSeq, activity });
    else expect(loaded).toMatchObject({ lastSeq: 900, oldestSeq: 0, activity: [] });
  });
});

describe("mid-turn reset keeps usage accounting (#4530)", () => {
  const usage = (amount: number): AcpEvent => ({
    UsageUpdated: { usage: { used: 1, size: 200_000, cost: { amount, currency: "USD" } } },
  });
  const frame = (seq: number, event: AcpEvent) => ({ session_id: "s", seq, event });
  const reopened = () => {
    const activity = [{ id: "t", kind: "tool_start", text: "t", at: "t" }];
    const sessionUsage = { used: 1, size: 200_000, cost: { amount: 2, currency: "USD" } };
    const state = {
      ...emptyAcpState(),
      lastSeq: 900,
      oldestSeq: 40,
      activity,
      sessionUsage,
      usageBaseline: { cost: 10 },
    };
    writeEntry("usage", state);
    return loadPersistedState("usage")!;
  };

  it("folds a boundary after the cached cursor into the cached usage", () => {
    const state = [frame(901, "ConversationCompacted"), frame(902, usage(13))].reduce(applyEvent, reopened());
    expect(state.sessionUsage?.cost?.amount).toBeCloseTo(1, 6);
  });

  it("drops a boundary the cache already accounted for", () => {
    const state = [frame(800, "ConversationCompacted"), frame(801, usage(12))].reduce(applyEvent, reopened());
    expect(state).toMatchObject({ usageBaseline: { cost: 10 }, sessionUsage: { cost: { amount: 2 } } });
  });
});

describe("cacheGet mid-turn in-memory entry (#4530)", () => {
  it("resets an in-memory entry without a prompt but keeps queued prompts", () => {
    const queued = [{ id: "q1", text: "next", queuedAt: "2026-01-01T00:00:00Z" }];
    const activity = [{ id: "t", kind: "tool_start", text: "t", at: "t" }] as ActivityRow[];
    cacheSet("mem", { ...emptyAcpState(), lastSeq: 900, oldestSeq: 40, activity, queuedPrompts: queued });
    expect(cacheGet("mem")).toMatchObject({ lastSeq: 900, oldestSeq: 0, activity: [], queuedPrompts: queued });
  });
});

describe("cacheSet persistence throttle", () => {
  const withSeq = (lastSeq: number): AcpState => ({ ...emptyAcpState(), lastSeq });
  const stored = (id: string) =>
    (JSON.parse(localStorage.getItem(key(id)) ?? "null") as { state: AcpState } | null)?.state;
  const setItem = () => vi.spyOn(Storage.prototype, "setItem");

  beforeEach(() => vi.useFakeTimers());
  afterEach(() => {
    clearAcpCache();
    vi.useRealTimers();
  });

  it("updates memory and notifies synchronously but persists once per window with the latest state", () => {
    const spy = setItem();
    const { result } = renderHook(() => useBackgroundAgents("thr"));
    act(() => cacheSet("thr", withSeq(1)));
    act(() => cacheSet("thr", { ...withSeq(2), backgroundAgents: [{ id: "a" } as unknown as BackgroundAgent] }));
    expect(result.current).toHaveLength(1);
    expect(cacheGet("thr")?.lastSeq).toBe(2);
    expect(spy).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1999);
    expect(spy).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(spy).toHaveBeenCalledTimes(1);
    expect(stored("thr")?.lastSeq).toBe(2);
    cacheSet("thr", withSeq(3));
    vi.advanceTimersByTime(2000);
    expect(spy).toHaveBeenCalledTimes(2);
    expect(stored("thr")?.lastSeq).toBe(3);
  });

  it("reads unflushed state after in-memory eviction, not the older persisted entry", () => {
    writeEntry("evicted", withSeq(1));
    cacheSet("evicted", withSeq(2));
    for (let i = 0; i < 32; i++) cacheSet(`other-${i}`, withSeq(i));
    expect(stored("evicted")?.lastSeq).toBe(1);
    expect(cacheGet("evicted")?.lastSeq).toBe(2);
  });

  it("flushes on pagehide and on a hidden visibilitychange, not a visible one", () => {
    const visibility = vi.spyOn(document, "visibilityState", "get");
    cacheSet("hide", withSeq(5));
    visibility.mockReturnValue("visible");
    document.dispatchEvent(new Event("visibilitychange"));
    expect(stored("hide")).toBeUndefined();
    visibility.mockReturnValue("hidden");
    document.dispatchEvent(new Event("visibilitychange"));
    expect(stored("hide")?.lastSeq).toBe(5);
    cacheSet("gone", withSeq(6));
    window.dispatchEvent(new Event("pagehide"));
    expect(stored("gone")?.lastSeq).toBe(6);
    const spy = setItem();
    vi.advanceTimersByTime(2000);
    expect(spy).not.toHaveBeenCalled();
  });

  it("flushPersist(id) writes only that session", () => {
    cacheSet("one", withSeq(1));
    cacheSet("two", withSeq(2));
    flushPersist("one");
    expect(stored("one")?.lastSeq).toBe(1);
    expect(stored("two")).toBeUndefined();
    flushPersist();
    expect(stored("two")?.lastSeq).toBe(2);
  });

  it("skips a flush whose snapshot is older than the stored entry (two tabs)", () => {
    vi.setSystemTime(1_000_000);
    cacheSet("tabs", withSeq(1));
    vi.setSystemTime(1_000_500);
    const queued = [{ id: "q1", text: "hello" }] as unknown as AcpState["queuedPrompts"];
    writeEntry("tabs", { ...withSeq(2), queuedPrompts: queued }, 1_000_500);
    vi.setSystemTime(1_002_000);
    flushPersist("tabs");
    expect(stored("tabs")?.lastSeq).toBe(2);
    expect(stored("tabs")?.queuedPrompts).toHaveLength(1);
  });

  it("persists the snapshot when the stored entry is older or absent", () => {
    vi.setSystemTime(2_000_000);
    writeEntry("older", withSeq(1), 1_999_000);
    cacheSet("older", withSeq(2));
    cacheSet("absent", withSeq(3));
    flushPersist();
    expect(stored("older")?.lastSeq).toBe(2);
    expect(stored("absent")?.lastSeq).toBe(3);
  });

  it("stamps the entry with capture time so an older capture cannot shadow a newer one", () => {
    vi.setSystemTime(3_000_000);
    cacheSet("stamp", withSeq(1));
    vi.setSystemTime(3_001_900);
    flushPersist("stamp");
    expect(JSON.parse(localStorage.getItem(key("stamp"))!).savedAt).toBe(3_000_000);
    // Other tab captured between this capture and its flush, then flushed first.
    vi.setSystemTime(3_002_000);
    cacheSet("order", withSeq(1));
    vi.setSystemTime(3_002_500);
    writeEntry("order", withSeq(2), 3_002_300);
    flushPersist("order");
    expect(stored("order")?.lastSeq).toBe(2);
    // This capture is newer than the stored entry: it wins even though the other flushed earlier.
    vi.setSystemTime(3_003_000);
    cacheSet("order2", withSeq(3));
    writeEntry("order2", withSeq(2), 3_002_900);
    vi.setSystemTime(3_004_000);
    flushPersist("order2");
    expect(stored("order2")?.lastSeq).toBe(3);
  });

  it("clearAcpCache cancels pending writes", () => {
    cacheSet("cleared", withSeq(1));
    cacheSet("kept", withSeq(2));
    clearAcpCache("cleared");
    vi.advanceTimersByTime(2000);
    expect(stored("cleared")).toBeUndefined();
    expect(stored("kept")?.lastSeq).toBe(2);
    cacheSet("all", withSeq(3));
    clearAcpCache();
    vi.advanceTimersByTime(2000);
    expect(stored("all")).toBeUndefined();
  });
});

describe("pending queued rows", () => {
  const T = 5_000_000;
  const row = (id: string, extra: Record<string, unknown> = {}) => ({
    id,
    text: id,
    queuedAt: "2026-01-01T00:00:00.000Z",
    pending: true,
    ...extra,
  });
  const withRows = (...rows: ReturnType<typeof row>[]): AcpState => ({
    ...emptyAcpState(),
    lastSeq: 1,
    queuedPrompts: rows,
  });
  const pendingKeys = (id: string) =>
    Object.keys(localStorage)
      .filter((k) => k.startsWith(`${PENDING_KEY_PREFIX}${id}:`))
      .map((k) => k.slice(`${PENDING_KEY_PREFIX}${id}:`.length))
      .sort();
  const writeRow = (id: string, r: ReturnType<typeof row>) =>
    localStorage.setItem(`${PENDING_KEY_PREFIX}${id}:${r.id}`, JSON.stringify({ savedAt: T, row: r }));
  const loadedIds = (id: string) => loadPersistedState(id)?.queuedPrompts.map((q) => q.id);
  const loadedPending = (id: string) =>
    loadPersistedState(id)?.queuedPrompts.map(({ id: rowId, text }) => ({ id: rowId, text }));

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(T);
  });
  afterEach(() => {
    clearAcpCache();
    vi.useRealTimers();
  });

  it("writes an enqueued row at once, and never into the throttled transcript", () => {
    cacheSet("pr-now", withRows(row("a")));
    expect(pendingKeys("pr-now")).toEqual(["a"]);
    expect(localStorage.getItem(key("pr-now"))).toBeNull();
    flushPersist("pr-now");
    expect(JSON.parse(localStorage.getItem(key("pr-now"))!).state.queuedPrompts).toEqual([]);
  });

  it("loads pending rows with no transcript, as after an eviction", () => {
    cacheSet("pr-evict", withRows(row("a")));
    expect(localStorage.getItem(key("pr-evict"))).toBeNull();
    expect(loadedIds("pr-evict")).toEqual(["a"]);
    expect(loadPersistedState("pr-none")).toBeUndefined();
  });

  it("keeps the row when a stale snapshot without it is stored, in either order and on a same-millisecond tie", () => {
    cacheSet("pr-o1", withRows(row("a")));
    flushPersist("pr-o1");
    writeEntry("pr-o1", { ...withRows(), lastSeq: 1 }, T);
    expect(loadedIds("pr-o1")).toEqual(["a"]);
    writeEntry("pr-o2", { ...withRows(), lastSeq: 1 }, T);
    cacheSet("pr-o2", withRows(row("a")));
    flushPersist("pr-o2");
    expect(loadedIds("pr-o2")).toEqual(["a"]);
  });

  it("lets removal or confirmation win over a stale transcript that still lists the row", () => {
    cacheSet("pr-gone", withRows(row("a")));
    cacheSet("pr-gone", withRows());
    expect(pendingKeys("pr-gone")).toEqual([]);
    // A stale snapshot from before the removal is stored afterwards: it must not bring the row back.
    persistState("pr-gone", withRows(row("a")), T);
    expect(loadedIds("pr-gone")).toEqual([]);
  });

  it("does not let concurrent additions replace each other, and removes only the rows this tab removed", () => {
    cacheSet("pr-tabs", withRows(row("a")));
    writeRow("pr-tabs", row("b"));
    cacheSet("pr-tabs", withRows(row("a"), row("c")));
    expect(pendingKeys("pr-tabs")).toEqual(["a", "b", "c"]);
    cacheSet("pr-tabs", withRows(row("a", { pending: false }), row("c")));
    expect(pendingKeys("pr-tabs")).toEqual(["b", "c"]);
  });

  it("persists an edit to a pending row, without recreating a row another tab removed", () => {
    cacheSet("pr-edit", withRows(row("a")));
    cacheSet("pr-edit", withRows(row("a", { text: "edited" })));
    expect(loadedPending("pr-edit")).toEqual([{ id: "a", text: "edited" }]);
    localStorage.removeItem(`${PENDING_KEY_PREFIX}pr-edit:a`);
    cacheSet("pr-edit", withRows(row("a", { text: "edited again" })));
    expect(pendingKeys("pr-edit")).toEqual([]);
  });

  it("migrates pending rows from an older transcript before a flush strips them", () => {
    writeEntry("pr-up", withRows(row("a")), T);
    expect(loadedIds("pr-up")).toEqual(["a"]);
    expect(pendingKeys("pr-up")).toEqual(["a"]);
    persistState("pr-up", withRows(row("a")), T + 1);
    expect(loadedIds("pr-up")).toEqual(["a"]);
  });

  it("keeps a legacy row in the transcript until its own entry is durable", () => {
    writeEntry("pr-full", withRows(row("a")), T);
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation((k) => {
      if (k.startsWith(PENDING_KEY_PREFIX)) throw new DOMException("full", "QuotaExceededError");
    });
    expect(loadedIds("pr-full")).toEqual(["a"]);
    spy.mockRestore();
    expect(pendingKeys("pr-full")).toEqual([]);
    persistState("pr-full", withRows(row("a")), T + 1);
    expect(loadedIds("pr-full")).toEqual(["a"]);
  });

  it("does not adopt a legacy row again once another tab confirmed it", async () => {
    writeEntry("pr-done", withRows(row("a")), T);
    expect(loadedIds("pr-done")).toEqual(["a"]);
    cacheSet("pr-done", withRows(row("a", { pending: false })));
    // A second tab (its own module state) opens before the transcript flush and still sees the legacy copy.
    vi.resetModules();
    const tab2 = await import("./stateCache");
    expect(tab2.loadPersistedState("pr-done")?.queuedPrompts).toEqual([]);
    expect(pendingKeys("pr-done")).toEqual([]);
  });

  it("keeps a row's entry until its terminal mark is stored, so a failed mark is retried", async () => {
    writeEntry("pr-mark", withRows(row("a")), T);
    expect(loadedIds("pr-mark")).toEqual(["a"]);
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation((k) => {
      if (k.startsWith(PENDING_DONE_KEY_PREFIX)) throw new DOMException("full", "QuotaExceededError");
    });
    cacheSet("pr-mark", withRows(row("a", { pending: false })));
    spy.mockRestore();
    expect(pendingKeys("pr-mark")).toEqual(["a"]);
    cacheSet("pr-mark", withRows(row("a", { pending: false })));
    expect(pendingKeys("pr-mark")).toEqual([]);
    vi.resetModules();
    const tab2 = await import("./stateCache");
    expect(tab2.loadPersistedState("pr-mark")?.queuedPrompts).toEqual([]);
  });

  it("retries a pending write that failed on quota", () => {
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementationOnce(() => {
      throw new DOMException("The quota has been exceeded.", "QuotaExceededError");
    });
    cacheSet("pr-quota", withRows(row("a")));
    expect(pendingKeys("pr-quota")).toEqual([]);
    cacheSet("pr-quota", withRows(row("a")));
    expect(pendingKeys("pr-quota")).toEqual(["a"]);
    spy.mockRestore();
  });

  it("never stores attachment rows, and clearAcpCache removes the entries", () => {
    cacheSet("pr-att", withRows(row("a", { attachments: [{ name: "x" }] }), row("b")));
    expect(pendingKeys("pr-att")).toEqual(["b"]);
    clearAcpCache("pr-att");
    expect(pendingKeys("pr-att")).toEqual([]);
  });
});

describe("eviction on quota (#1345)", () => {
  it("evicts the oldest acp-state entry and retries the write", () => {
    writeEntry("sess-old", undefined, Date.now() - DAY_MS);
    writeEntry("sess-new");
    vi.spyOn(Storage.prototype, "setItem").mockImplementationOnce(() => {
      throw quotaError();
    });
    persistState("sess-current", emptyAcpState());
    expect(localStorage.getItem(key("sess-old"))).toBeNull();
    expect(localStorage.getItem(key("sess-new"))).not.toBeNull();
    expect(localStorage.getItem(CURRENT)).not.toBeNull();
  });

  it("retries once only, silently", () => {
    writeEntry("sess-old", undefined, Date.now() - DAY_MS);
    const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw quotaError();
    });
    expect(() => persistState("sess-current", emptyAcpState())).not.toThrow();
    expect(setItem).toHaveBeenCalledTimes(2);
  });

  it("never evicts drafts, unrelated keys, or the key being written", () => {
    localStorage.setItem("acp:draft:sess-old", "draft body");
    localStorage.setItem("aoe-resolved-theme", "themedata");
    writeEntry("sess-current", undefined, Date.now() - 2 * DAY_MS);
    expect(evictOldestPersistedAcpState(CURRENT)).toBe(false);

    writeEntry("sess-old", undefined, Date.now() - DAY_MS);
    expect(evictOldestPersistedAcpState(CURRENT)).toBe(true);
    expect(localStorage.getItem(key("sess-old"))).toBeNull();
    expect(localStorage.getItem("acp:draft:sess-old")).toBe("draft body");
    expect(localStorage.getItem("aoe-resolved-theme")).toBe("themedata");
    expect(localStorage.getItem(CURRENT)).not.toBeNull();
  });

  it("prefers a corrupt entry over an older valid one", () => {
    writeEntry("sess-valid-old", undefined, Date.now() - DAY_MS);
    localStorage.setItem(key("sess-corrupt"), "not valid json{{{");
    evictOldestPersistedAcpState(CURRENT);
    expect(localStorage.getItem(key("sess-corrupt"))).toBeNull();
    expect(localStorage.getItem(key("sess-valid-old"))).not.toBeNull();
  });
});

describe("persistState (#1833)", () => {
  const readQueue = (id: string) =>
    (JSON.parse(localStorage.getItem(key(id))!) as { state: AcpState }).state.queuedPrompts.map((q) => q.id);

  it("drops attachment-bearing queued rows and writes no base64 bytes", () => {
    const attachments = [{ kind: "image" as const, mimeType: "image/png", dataB64: "QUJDREVG", name: "shot.png" }];
    persistState("strip", {
      ...emptyAcpState(),
      queuedPrompts: [
        { id: "q1", text: "plain text", queuedAt: "t" },
        { id: "q2", text: "with image", queuedAt: "t", attachments },
      ],
    });
    expect(localStorage.getItem(key("strip"))).not.toContain("QUJDREVG");
    expect(readQueue("strip")).toEqual(["q1"]);
  });

  it("drops the optimistic overlay, in-flight prompt ids and restart markers", () => {
    const row = { id: "o1", kind: "user_prompt" as const, text: "x", at: "t" };
    persistState("overlay", {
      ...emptyAcpState(),
      optimisticRows: [row],
      inflightPromptIds: ["o1"],
      restartInFlight: true,
      restartStoppedSent: true,
      restartFailure: "Server returned 500.",
      retriedError: "boom",
    });
    const { state } = JSON.parse(localStorage.getItem(key("overlay"))!) as { state: AcpState };
    expect([
      state.optimisticRows,
      state.inflightPromptIds,
      state.restartInFlight,
      state.restartStoppedSent,
      state.restartFailure,
      state.retriedError,
    ]).toEqual([[], [], false, false, null, null]);
  });

  it("drops what a failed Retry retried, so a reload reads as a first failure", () => {
    persistState("retried", { ...emptyAcpState(), startupError: "worse", retriedError: "boom" });
    const { state } = JSON.parse(localStorage.getItem(key("retried"))!) as { state: AcpState };
    expect([state.startupError, state.retriedError]).toEqual(["worse", null]);
  });

  it("keeps a startup error's cause, so a reload shows the same banner", () => {
    const cause = { kind: "container_missing" } as const;
    persistState("gone", { ...emptyAcpState(), startupError: "removed", startupCause: cause });
    expect(loadPersistedState("gone")?.startupCause).toEqual(cause);
  });
});

describe("clearAcpCache", () => {
  it("drops one entry or every acp-state entry, leaving unrelated keys and swallowing storage errors", () => {
    writeEntry("sess-a");
    writeEntry("sess-b");
    localStorage.setItem("unrelated:key", "x");
    clearAcpCache("sess-a");
    expect(localStorage.getItem(key("sess-a"))).toBeNull();
    expect(localStorage.getItem(key("sess-b"))).not.toBeNull();
    clearAcpCache();
    expect(localStorage.getItem(key("sess-b"))).toBeNull();
    expect(localStorage.getItem("unrelated:key")).toBe("x");

    vi.spyOn(localStorage, "removeItem").mockImplementation(() => {
      throw new Error("denied");
    });
    expect(() => clearAcpCache("sess-quota")).not.toThrow();
  });
});

describe("useBackgroundAgents", () => {
  it("follows cache writes for its session without a socket", () => {
    const { result, unmount } = renderHook(() => useBackgroundAgents("sess-bg"));
    expect(result.current).toEqual([]);
    const agents = [{ id: "agent-1" } as unknown as BackgroundAgent];
    act(() => cacheSet("sess-bg", { ...emptyAcpState(), backgroundAgents: agents }));
    expect(result.current).toBe(agents);
    unmount();
    clearAcpCache("sess-bg");
  });
});

describe("useBackgroundShells", () => {
  it("follows cache writes for its session without a socket", () => {
    const { result, unmount } = renderHook(() => useBackgroundShells("sess-sh"));
    expect(result.current).toEqual([]);
    const shells: BackgroundShell[] = [
      { taskId: "t1", command: "sleep 9", startedAt: null, endedAt: null, finished: false },
    ];
    act(() => cacheSet("sess-sh", { ...emptyAcpState(), backgroundShells: shells }));
    expect(result.current).toBe(shells);
    unmount();
    clearAcpCache("sess-sh");
  });
});

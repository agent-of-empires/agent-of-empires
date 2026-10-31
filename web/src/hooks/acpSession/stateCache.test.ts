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
} from "../../lib/acpTypes";
import { STORAGE_KEY_PREFIX } from "../../lib/acpStateStorage";
import {
  cacheGet,
  cacheSet,
  clearAcpCache,
  evictOldestPersistedAcpState,
  loadPersistedState,
  persistState,
  useBackgroundAgents,
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
  it.each(["pendingElicitations", "oldestSeq", "interactionError"])(
    "backfills %s missing from an older entry",
    (field) => {
      const legacy: Record<string, unknown> = { ...emptyAcpState() };
      delete legacy[field];
      writeEntry("legacy", legacy);
      expect(loadPersistedState("legacy")).toMatchObject({
        [field]: (emptyAcpState() as unknown as Record<string, unknown>)[field],
      });
    },
  );

  it("preserves stored values", () => {
    const elicitation = { nonce: "e-1", message: "Pick", questions: [], resolved: null };
    const activity = [{ id: "u", kind: "user_prompt", text: "u", at: "t" }];
    writeEntry("seq", { ...emptyAcpState(), oldestSeq: 42, activity, pendingElicitations: [elicitation] });
    const loaded = loadPersistedState("seq");
    expect(loaded?.oldestSeq).toBe(42);
    expect(loaded?.pendingElicitations.map((e) => e.nonce)).toEqual(["e-1"]);
  });

  it("persists canonical failures without persisting transient interaction errors", () => {
    const state = applyEvent(emptyAcpState(), {
      session_id: "s",
      seq: 1,
      event: { PromptRuntimeError: { message: "canonical failure" } },
    });
    persistState("failed", {
      ...state,
      interactionError: { message: "temporary request failure", target: { kind: "elicitation", nonce: "e-1" } },
    });
    const loaded = loadPersistedState("failed");
    expect(loaded?.lastError).toBe("canonical failure");
    expect(loaded?.lastErrorEvidence).toBeNull();
    expect(loaded?.interactionError).toBeNull();
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

  it("drops the optimistic overlay and in-flight prompt ids", () => {
    const row = { id: "o1", kind: "user_prompt" as const, text: "x", at: "t" };
    persistState("overlay", { ...emptyAcpState(), optimisticRows: [row], inflightPromptIds: ["o1"] });
    const { state } = JSON.parse(localStorage.getItem(key("overlay"))!) as { state: AcpState };
    expect([state.optimisticRows, state.inflightPromptIds]).toEqual([[], []]);
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

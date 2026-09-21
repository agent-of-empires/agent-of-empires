// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { emptyAcpState, type AcpState, type BackgroundAgent } from "../../lib/acpTypes";
import { STORAGE_KEY_PREFIX } from "../../lib/acpStateStorage";
import {
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
  it.each(["pendingElicitations", "oldestSeq"])("backfills %s missing from an older entry", (field) => {
    const legacy: Record<string, unknown> = { ...emptyAcpState() };
    delete legacy[field];
    writeEntry("legacy", legacy);
    expect(loadPersistedState("legacy")).toMatchObject({
      [field]: (emptyAcpState() as unknown as Record<string, unknown>)[field],
    });
  });

  it("preserves stored values", () => {
    const elicitation = { nonce: "e-1", message: "Pick", questions: [], resolved: null };
    writeEntry("seq", { ...emptyAcpState(), oldestSeq: 42, pendingElicitations: [elicitation] });
    const loaded = loadPersistedState("seq");
    expect(loaded?.oldestSeq).toBe(42);
    expect(loaded?.pendingElicitations.map((e) => e.nonce)).toEqual(["e-1"]);
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
  it("drops one entry or every acp-state entry, leaving unrelated keys", () => {
    writeEntry("sess-a");
    writeEntry("sess-b");
    localStorage.setItem("unrelated:key", "x");
    clearAcpCache("sess-a");
    expect(localStorage.getItem(key("sess-a"))).toBeNull();
    expect(localStorage.getItem(key("sess-b"))).not.toBeNull();
    clearAcpCache();
    expect(localStorage.getItem(key("sess-b"))).toBeNull();
    expect(localStorage.getItem("unrelated:key")).toBe("x");
  });

  it("swallows storage errors", () => {
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

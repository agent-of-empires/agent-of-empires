// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  __resetDraftPersistFailureNotifications,
  clearDraft,
  clearDraftAttachments,
  getDraft,
  getDraftAttachments,
  hasDraft,
  hasDraftAttachments,
  setDraft,
  setDraftAttachments,
  subscribeDrafts,
  sweepOrphanDrafts,
} from "./acpDrafts";
import type { PromptAttachmentInput } from "./acpTypes";
import { toastBus } from "./toastBus";

const img = (dataB64 = "AAAA", name?: string): PromptAttachmentInput => ({
  kind: "image",
  mimeType: "image/png",
  dataB64,
  ...(name ? { name } : {}),
});

const throwOn = (method: "getItem" | "setItem" | "removeItem" | "key", error: unknown = new Error("blocked")) =>
  vi.spyOn(Storage.prototype, method).mockImplementation(() => {
    throw error;
  });
const quotaFull = () => throwOn("setItem", new DOMException("The quota has been exceeded.", "QuotaExceededError"));

function toastErrors(): string[] {
  const errors: string[] = [];
  toastBus.handler = {
    push: (msg, kind) => {
      if (kind === "error") errors.push(msg);
    },
    error: (msg) => {
      errors.push(msg);
    },
    info: () => {},
    openLink: () => {},
  };
  return errors;
}

const unsubs: (() => void)[] = [];
function listen(filter: string[] | null) {
  const cb = vi.fn();
  unsubs.push(subscribeDrafts(cb, filter && new Set(filter)));
  return cb;
}
const storageEvent = (key: string | null) => window.dispatchEvent(new StorageEvent("storage", { key, newValue: "x" }));

beforeEach(() => {
  window.localStorage.clear();
  __resetDraftPersistFailureNotifications();
});

afterEach(() => {
  for (const unsub of unsubs.splice(0)) unsub();
  window.localStorage.clear();
  vi.restoreAllMocks();
  toastBus.handler = null;
});

describe("text drafts", () => {
  it("round-trip per session and remove the key when emptied", () => {
    expect([getDraft("s-1"), hasDraft("s-1")]).toEqual(["", false]);
    setDraft("s-1", "one");
    setDraft("s-2", "two");
    expect([getDraft("s-1"), getDraft("s-2"), hasDraft("s-1")]).toEqual(["one", "two", true]);
    setDraft("s-1", "");
    expect([getDraft("s-1"), hasDraft("s-1")]).toEqual(["", false]);
    expect(localStorage.getItem("acp:draft:s-1")).toBeNull();
  });

  it("clearDraft removes the key and notifies; it is a no-op when absent", () => {
    setDraft("s-1", "x");
    const cb = listen(["s-1"]);
    clearDraft("s-1");
    expect(localStorage.getItem("acp:draft:s-1")).toBeNull();
    expect(cb).toHaveBeenCalledTimes(1);
    expect(() => clearDraft("s-missing")).not.toThrow();
  });

  it("degrades when localStorage throws", () => {
    throwOn("getItem");
    expect([getDraft("s-1"), hasDraft("s-1")]).toEqual(["", false]);
    vi.restoreAllMocks();
    throwOn("setItem");
    expect(() => setDraft("s-1", "x")).not.toThrow();
  });
});

describe("subscribeDrafts", () => {
  it("scopes same-tab writes to the filter and stops after unsubscribe", () => {
    const filtered = listen(["s-1"]);
    const wildcard = listen(null);
    setDraft("s-1", "a");
    setDraft("s-7", "b");
    expect(filtered).toHaveBeenCalledTimes(1);
    expect(wildcard).toHaveBeenCalledTimes(2);
    for (const unsub of unsubs.splice(0)) unsub();
    setDraft("s-1", "c");
    expect(filtered).toHaveBeenCalledTimes(1);
  });

  it.each<[string | null, number]>([
    ["acp:draft:s-1", 1],
    ["acp:draft-attachments:s-1", 1],
    ["acp:draft:s-other", 0],
    ["some-other-key", 0],
    [null, 1],
  ])("a cross-tab storage event for %s fires %s times", (key, calls) => {
    const cb = listen(["s-1"]);
    storageEvent(key);
    expect(cb).toHaveBeenCalledTimes(calls);
  });

  it("notifies filtered subscribers on attachment writes", () => {
    const cb = listen(["s-1"]);
    setDraftAttachments("s-1", [img()]);
    expect(cb).toHaveBeenCalledTimes(1);
  });
});

describe("sweepOrphanDrafts", () => {
  it("removes text and attachment drafts for inactive sessions only", () => {
    setDraft("s-keep", "alive");
    setDraftAttachments("s-keep", [img("keep")]);
    setDraft("s-orphan", "gone");
    setDraftAttachments("s-orphan", [img("gone")]);
    localStorage.setItem("aoe:other", "untouched");
    sweepOrphanDrafts(new Set(["s-keep"]));
    expect(getDraft("s-keep")).toBe("alive");
    expect(getDraftAttachments("s-keep")).toEqual([img("keep")]);
    expect(localStorage.getItem("acp:draft:s-orphan")).toBeNull();
    expect(localStorage.getItem("acp:draft-attachments:s-orphan")).toBeNull();
    expect(localStorage.getItem("aoe:other")).toBe("untouched");
  });

  it("notifies once only when something was removed", () => {
    setDraft("s-keep", "alive");
    const cb = listen(null);
    sweepOrphanDrafts(new Set(["s-keep"]));
    expect(cb).not.toHaveBeenCalled();
    setDraft("s-a", "a");
    setDraft("s-b", "b");
    cb.mockClear();
    sweepOrphanDrafts(new Set());
    expect(cb).toHaveBeenCalledTimes(1);
  });

  it("swallows localStorage iteration errors", () => {
    setDraft("s-orphan", "gone");
    throwOn("key");
    expect(() => sweepOrphanDrafts(new Set())).not.toThrow();
  });
});

describe("attachment drafts (#2493)", () => {
  it("round-trip per session and light hasDraft without text", () => {
    expect([getDraftAttachments("s-1"), hasDraftAttachments("s-1"), hasDraft("s-1")]).toEqual([[], false, false]);
    setDraftAttachments("s-1", [img("ONE", "shot.png")]);
    setDraftAttachments("s-2", [img("TWO")]);
    expect(getDraftAttachments("s-1")).toEqual([img("ONE", "shot.png")]);
    expect(getDraftAttachments("s-2")).toEqual([img("TWO")]);
    expect([hasDraftAttachments("s-1"), hasDraft("s-1")]).toEqual([true, true]);
  });

  it.each([
    ["an empty array", () => setDraftAttachments("s-1", [])],
    ["clearDraftAttachments", () => clearDraftAttachments("s-1")],
  ])("%s removes the key", (_name, clear) => {
    setDraftAttachments("s-1", [img()]);
    clear();
    expect(localStorage.getItem("acp:draft-attachments:s-1")).toBeNull();
    expect(hasDraftAttachments("s-1")).toBe(false);
  });

  it("drops malformed entries and treats corrupt JSON as no draft", () => {
    localStorage.setItem("acp:draft-attachments:s-1", JSON.stringify([img("OK"), { kind: "image" }, "garbage", null]));
    expect(getDraftAttachments("s-1")).toEqual([img("OK")]);
    localStorage.setItem("acp:draft-attachments:s-1", "{not json");
    expect([getDraftAttachments("s-1"), hasDraftAttachments("s-1"), hasDraft("s-1")]).toEqual([[], false, false]);
  });

  it("removes the key on a failed write so a stale draft is never restored, toasting once", () => {
    const errors = toastErrors();
    setDraftAttachments("s-1", [img("OLD")]);
    quotaFull();
    setDraftAttachments("s-1", [img("NEW-TOO-BIG")]);
    setDraftAttachments("s-1", [img("BIGGER")]);
    vi.restoreAllMocks();
    expect(localStorage.getItem("acp:draft-attachments:s-1")).toBeNull();
    expect(errors).toEqual([expect.stringMatching(/storage full/i)]);
  });
});

describe("persist-failure toast dedupe (#1345)", () => {
  it("toasts once per session until a successful write re-arms it", () => {
    const errors = toastErrors();
    quotaFull();
    setDraft("sess-a", "x");
    setDraft("sess-a", "xy");
    setDraft("sess-b", "b");
    setDraft("sess-b", "bb");
    expect(errors).toHaveLength(2);
    expect(errors[0]).toMatch(/storage full/i);
    vi.restoreAllMocks();
    setDraft("sess-a", "xyz");
    expect(window.localStorage.getItem("acp:draft:sess-a")).toBe("xyz");
    quotaFull();
    setDraft("sess-a", "xyzw");
    expect(errors).toHaveLength(3);
  });

  it("does not toast for removals or successful writes", () => {
    const errors = toastErrors();
    setDraft("sess-a", "hello");
    throwOn("removeItem");
    setDraft("sess-a", "");
    expect(errors).toEqual([]);
  });
});

// @vitest-environment jsdom
//
// Two coexisting test suites for the structured view composer drafts module:
//
// 1. Storage + pub/sub contract for the "unsent draft" dot in the sidebar.
//    The listener-filter logic is hot path for sidebar re-renders; if it
//    drifts, every keystroke fans out to every entry.
// 2. Per-session toast dedupe on draft persistence failure (#1345). Drafts
//    are unsent user text, so a write failure must surface, but setDraft
//    fires on every keystroke and a naive toast would storm. Each session
//    toasts at most once per page lifetime, until a later successful write
//    clears its dedupe entry. Two failing sessions toast independently.
//    State-cache writes stay silent (handled in useStructuredView.ts); only
//    drafts trip this toast.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  __resetDraftPersistFailureNotifications,
  clearDraft,
  getDraft,
  hasDraft,
  setDraft,
  subscribeDrafts,
  sweepOrphanDrafts,
} from "./acpDrafts";
import { toastBus, type ToastApi } from "./toastBus";

function makeQuotaError(): DOMException {
  return new DOMException("The quota has been exceeded.", "QuotaExceededError");
}

function attachToastSpy(): ToastApi & {
  errors: string[];
  infos: string[];
} {
  const errors: string[] = [];
  const infos: string[] = [];
  const handler: ToastApi & { errors: string[]; infos: string[] } = {
    push(msg, kind) {
      if (kind === "error") errors.push(msg);
      else infos.push(msg);
    },
    error(msg) {
      errors.push(msg);
    },
    info(msg) {
      infos.push(msg);
    },
    errors,
    infos,
  };
  toastBus.handler = handler;
  return handler;
}

beforeEach(() => {
  window.localStorage.clear();
  __resetDraftPersistFailureNotifications();
});

afterEach(() => {
  window.localStorage.clear();
  vi.restoreAllMocks();
  toastBus.handler = null;
});

describe("getDraft / setDraft", () => {
  it("returns empty string when no draft is persisted", () => {
    expect(getDraft("s-1")).toBe("");
  });

  it("round-trips a written draft", () => {
    setDraft("s-1", "hello world");
    expect(getDraft("s-1")).toBe("hello world");
  });

  it("scopes drafts per session id", () => {
    setDraft("s-1", "one");
    setDraft("s-2", "two");
    expect(getDraft("s-1")).toBe("one");
    expect(getDraft("s-2")).toBe("two");
  });

  it("empty text removes the key entirely", () => {
    setDraft("s-1", "filled");
    setDraft("s-1", "");
    expect(getDraft("s-1")).toBe("");
    expect(localStorage.getItem("acp:draft:s-1")).toBeNull();
  });

  it("returns empty string when localStorage.getItem throws", () => {
    const spy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(getDraft("s-1")).toBe("");
    spy.mockRestore();
  });

  it("setDraft swallows localStorage write errors", () => {
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    expect(() => setDraft("s-1", "x")).not.toThrow();
    spy.mockRestore();
  });
});

describe("hasDraft", () => {
  it("returns false for an empty session", () => {
    expect(hasDraft("s-1")).toBe(false);
  });

  it("returns true once a non-empty draft is written", () => {
    setDraft("s-1", "x");
    expect(hasDraft("s-1")).toBe(true);
  });

  it("returns false after clearing a draft", () => {
    setDraft("s-1", "x");
    setDraft("s-1", "");
    expect(hasDraft("s-1")).toBe(false);
  });

  it("returns false when localStorage throws", () => {
    const spy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(hasDraft("s-1")).toBe(false);
    spy.mockRestore();
  });
});

describe("subscribeDrafts pub/sub", () => {
  it("fires for setDraft writes on the listener's filter set", () => {
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, new Set(["s-1"]));
    setDraft("s-1", "hello");
    expect(cb).toHaveBeenCalledTimes(1);
    unsub();
  });

  it("does not fire for sessions outside the filter set", () => {
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, new Set(["s-1"]));
    setDraft("s-2", "hello");
    expect(cb).not.toHaveBeenCalled();
    unsub();
  });

  it("fires for any draft change when filter is null", () => {
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, null);
    setDraft("s-1", "a");
    setDraft("s-7", "b");
    expect(cb).toHaveBeenCalledTimes(2);
    unsub();
  });

  it("unsubscribe stops further notifications", () => {
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, null);
    unsub();
    setDraft("s-1", "x");
    expect(cb).not.toHaveBeenCalled();
  });

  it("cross-tab storage event for the matching key fires the listener", () => {
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, new Set(["s-1"]));
    window.dispatchEvent(
      new StorageEvent("storage", {
        key: "acp:draft:s-1",
        newValue: "x",
      }),
    );
    expect(cb).toHaveBeenCalledTimes(1);
    unsub();
  });

  it("cross-tab storage event for an unrelated key is ignored", () => {
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, new Set(["s-1"]));
    window.dispatchEvent(
      new StorageEvent("storage", {
        key: "some-other-key",
        newValue: "x",
      }),
    );
    expect(cb).not.toHaveBeenCalled();
    unsub();
  });

  it("storage event for a non-filtered session does not fire", () => {
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, new Set(["s-1"]));
    window.dispatchEvent(
      new StorageEvent("storage", {
        key: "acp:draft:s-other",
        newValue: "x",
      }),
    );
    expect(cb).not.toHaveBeenCalled();
    unsub();
  });

  it("storage event with null key (whole-storage wipe) fires unconditionally", () => {
    const cbFiltered = vi.fn();
    const cbWildcard = vi.fn();
    const unsub1 = subscribeDrafts(cbFiltered, new Set(["s-1"]));
    const unsub2 = subscribeDrafts(cbWildcard, null);
    window.dispatchEvent(new StorageEvent("storage", { key: null, newValue: null }));
    expect(cbFiltered).toHaveBeenCalledTimes(1);
    expect(cbWildcard).toHaveBeenCalledTimes(1);
    unsub1();
    unsub2();
  });
});

describe("clearDraft", () => {
  it("removes the persisted key", () => {
    setDraft("s-1", "x");
    clearDraft("s-1");
    expect(localStorage.getItem("acp:draft:s-1")).toBeNull();
    expect(hasDraft("s-1")).toBe(false);
  });

  it("notifies filtered subscribers", () => {
    setDraft("s-1", "x");
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, new Set(["s-1"]));
    clearDraft("s-1");
    expect(cb).toHaveBeenCalledTimes(1);
    unsub();
  });

  it("is a no-op when no draft existed", () => {
    expect(() => clearDraft("s-missing")).not.toThrow();
    expect(getDraft("s-missing")).toBe("");
  });
});

describe("sweepOrphanDrafts", () => {
  it("removes drafts whose session id is not in the active set", () => {
    setDraft("s-keep", "alive");
    setDraft("s-orphan-1", "gone");
    setDraft("s-orphan-2", "also gone");
    sweepOrphanDrafts(new Set(["s-keep"]));
    expect(getDraft("s-keep")).toBe("alive");
    expect(localStorage.getItem("acp:draft:s-orphan-1")).toBeNull();
    expect(localStorage.getItem("acp:draft:s-orphan-2")).toBeNull();
  });

  it("leaves non-draft keys untouched", () => {
    localStorage.setItem("aoe:other", "untouched");
    localStorage.setItem("acp:draft:s-orphan", "gone");
    sweepOrphanDrafts(new Set());
    expect(localStorage.getItem("aoe:other")).toBe("untouched");
    expect(localStorage.getItem("acp:draft:s-orphan")).toBeNull();
  });

  it("fires a single wildcard notify when keys were removed", () => {
    setDraft("s-orphan", "gone");
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, null);
    sweepOrphanDrafts(new Set());
    expect(cb).toHaveBeenCalledTimes(1);
    unsub();
  });

  it("does not notify when nothing was removed", () => {
    setDraft("s-keep", "alive");
    const cb = vi.fn();
    const unsub = subscribeDrafts(cb, null);
    sweepOrphanDrafts(new Set(["s-keep"]));
    expect(cb).not.toHaveBeenCalled();
    unsub();
  });

  it("swallows localStorage iteration errors", () => {
    setDraft("s-orphan", "gone");
    const spy = vi.spyOn(Storage.prototype, "key").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(() => sweepOrphanDrafts(new Set())).not.toThrow();
    spy.mockRestore();
  });

  it("handles an empty active set", () => {
    setDraft("s-a", "a");
    setDraft("s-b", "b");
    sweepOrphanDrafts(new Set());
    expect(hasDraft("s-a")).toBe(false);
    expect(hasDraft("s-b")).toBe(false);
  });
});

describe("acpDrafts toast dedupe (#1345)", () => {
  it("fires exactly one toast per session when writes fail repeatedly", () => {
    const spy = attachToastSpy();
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw makeQuotaError();
    });

    setDraft("sess-a", "hello");
    setDraft("sess-a", "hello world");
    setDraft("sess-a", "hello world!");

    expect(spy.errors).toHaveLength(1);
    expect(spy.errors[0]).toMatch(/storage full/i);
  });

  it("clears dedupe after a successful write; later failure re-toasts", () => {
    const spy = attachToastSpy();

    // First storm: setItem throws.
    const setItemSpy = vi.spyOn(Storage.prototype, "setItem");
    setItemSpy.mockImplementation(() => {
      throw makeQuotaError();
    });
    setDraft("sess-a", "x");
    setDraft("sess-a", "xy");
    expect(spy.errors).toHaveLength(1);

    // Storage frees up. The next write succeeds and clears the flag.
    setItemSpy.mockRestore();
    setDraft("sess-a", "xyz"); // succeeds against real localStorage
    expect(window.localStorage.getItem("acp:draft:sess-a")).toBe("xyz");

    // Storage fills up again. The next failure must re-toast.
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw makeQuotaError();
    });
    setDraft("sess-a", "xyzw");
    expect(spy.errors).toHaveLength(2);
  });

  it("two failing sessions each get their own toast (no cross-suppression)", () => {
    const spy = attachToastSpy();
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw makeQuotaError();
    });

    setDraft("sess-a", "text-a");
    setDraft("sess-b", "text-b");
    setDraft("sess-a", "text-a-more");
    setDraft("sess-b", "text-b-more");

    expect(spy.errors).toHaveLength(2);
  });

  it("does not toast when text is empty (removal); no draft to lose", () => {
    const spy = attachToastSpy();
    vi.spyOn(Storage.prototype, "removeItem").mockImplementation(() => {
      throw makeQuotaError();
    });

    setDraft("sess-a", "");

    // Empty-text path goes through safeRemoveItem, which swallows the
    // throw silently. There is no unsent text at risk, so no toast.
    expect(spy.errors).toHaveLength(0);
  });

  it("does not toast when the write succeeds", () => {
    const spy = attachToastSpy();
    setDraft("sess-a", "hello");
    expect(window.localStorage.getItem("acp:draft:sess-a")).toBe("hello");
    expect(spy.errors).toHaveLength(0);
  });
});

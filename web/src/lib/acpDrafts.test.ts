// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  __resetDraftPersistFailureNotifications,
  clearDraft,
  getDraft,
  getDraftAttachments,
  hasDraft,
  hasDraftAttachments,
  notifyAttachmentRestoreFailure,
  setDraft,
  setDraftAttachments,
  subscribeDrafts,
  sweepOrphanDrafts,
} from "./acpDrafts";
import type { PromptAttachmentInput } from "./acpTypes";
import { deleteAttachments, listAttachmentSessionIds, saveAttachments } from "./attachmentStore";
import { toastBus } from "./toastBus";

// Lets a test hold a save in flight so queued-write ordering is exercised without sleeps.
const saveGate = vi.hoisted(() => ({ hold: null as Promise<void> | null, deleteHold: null as Promise<void> | null }));
vi.mock("./attachmentStore", async (importOriginal) => {
  const real = await importOriginal<typeof import("./attachmentStore")>();
  return {
    ...real,
    saveAttachments: async (...args: Parameters<typeof real.saveAttachments>) => {
      await saveGate.hold;
      return real.saveAttachments(...args);
    },
    deleteAttachments: async (...args: Parameters<typeof real.deleteAttachments>) => {
      await saveGate.deleteHold;
      return real.deleteAttachments(...args);
    },
  };
});

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

beforeEach(async () => {
  window.localStorage.clear();
  __resetDraftPersistFailureNotifications();
  await Promise.all((await listAttachmentSessionIds()).map((id) => deleteAttachments(id)));
});

// Sorted ids currently held in IndexedDB.
const storedIds = () => listAttachmentSessionIds().then((ids) => ids.sort());

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

  it("notifies filtered subscribers on attachment writes", async () => {
    const cb = listen(["s-1"]);
    await setDraftAttachments("s-1", [img()]);
    expect(cb).toHaveBeenCalledTimes(1);
  });
});

describe("sweepOrphanDrafts", () => {
  it("removes text and attachment drafts for inactive sessions only", async () => {
    setDraft("s-keep", "alive");
    await setDraftAttachments("s-keep", [img("keep")]);
    setDraft("s-orphan", "gone");
    await setDraftAttachments("s-orphan", [img("gone")]);
    localStorage.setItem("aoe:other", "untouched");
    sweepOrphanDrafts(new Set(["s-keep"]));
    expect(getDraft("s-keep")).toBe("alive");
    expect(await getDraftAttachments("s-keep")).toEqual([img("keep")]);
    expect(localStorage.getItem("acp:draft:s-orphan")).toBeNull();
    expect(localStorage.getItem("acp:draft-attachments:s-orphan")).toBeNull();
    expect(localStorage.getItem("aoe:other")).toBe("untouched");
    await vi.waitFor(async () => expect(await storedIds()).toEqual(["s-keep"]));
  });

  it("removes an IndexedDB record that has no marker", async () => {
    await saveAttachments("s-stray", [img("stray")]);
    sweepOrphanDrafts(new Set());
    await vi.waitFor(async () => expect(await storedIds()).toEqual([]));
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
  it("round-trip per session and light hasDraft without text", async () => {
    expect([await getDraftAttachments("s-1"), hasDraftAttachments("s-1"), hasDraft("s-1")]).toEqual([[], false, false]);
    await setDraftAttachments("s-1", [img("ONE", "shot.png")]);
    await setDraftAttachments("s-2", [img("TWO")]);
    expect(await getDraftAttachments("s-1")).toEqual([img("ONE", "shot.png")]);
    expect(await getDraftAttachments("s-2")).toEqual([img("TWO")]);
    expect([hasDraftAttachments("s-1"), hasDraft("s-1")]).toEqual([true, true]);
  });

  it("stores a photo far past the localStorage quota", async () => {
    const photo = img("A".repeat(8 * 1024 * 1024), "photo.jpg");
    const errors = toastErrors();
    await setDraftAttachments("s-1", [photo]);
    expect(errors).toEqual([]);
    expect(localStorage.getItem("acp:draft-attachments:s-1")).toBe("1");
    expect(await getDraftAttachments("s-1")).toEqual([photo]);
  });

  it("an empty array removes the marker and the stored record", async () => {
    await setDraftAttachments("s-1", [img()]);
    await setDraftAttachments("s-1", []);
    expect(localStorage.getItem("acp:draft-attachments:s-1")).toBeNull();
    expect(hasDraftAttachments("s-1")).toBe(false);
    expect(await storedIds()).toEqual([]);
  });

  it("drops malformed entries and treats a missing record as no draft", async () => {
    await saveAttachments("s-1", [img("OK"), { kind: "image" }, "garbage", null] as unknown as PromptAttachmentInput[]);
    localStorage.setItem("acp:draft-attachments:s-1", "1");
    expect(await getDraftAttachments("s-1")).toEqual([img("OK")]);
    localStorage.setItem("acp:draft-attachments:s-2", "1");
    expect(await getDraftAttachments("s-2")).toEqual([]);
  });

  it("reports a failed read as null, not as an empty draft", async () => {
    await setDraftAttachments("s-1", [img("KEEP")]);
    const get = vi.spyOn(IDBObjectStore.prototype, "get").mockImplementationOnce(() => {
      throw new DOMException("read failed", "UnknownError");
    });
    expect(await getDraftAttachments("s-1")).toBeNull();
    get.mockRestore();
    expect(await getDraftAttachments("s-1")).toEqual([img("KEEP")]);
  });

  it("drops a pre-IndexedDB JSON payload from the marker key on read", async () => {
    localStorage.setItem("acp:draft-attachments:s-1", JSON.stringify([img("OLD")]));
    expect(hasDraftAttachments("s-1")).toBe(false);
    expect(await getDraftAttachments("s-1")).toEqual([]);
    expect(localStorage.getItem("acp:draft-attachments:s-1")).toBeNull();
  });

  it("treats a marker that cannot be written as a failed save, leaving no orphan record", async () => {
    const errors = toastErrors();
    quotaFull();
    await setDraftAttachments("s-1", [img("SAVED")]);
    vi.restoreAllMocks();
    expect(await getDraftAttachments("s-1")).toEqual([]);
    expect(await storedIds()).toEqual([]);
    expect(errors).toEqual([expect.stringMatching(/storage full/i)]);
  });

  it("does not restore a record whose marker was removed, as when a delete aborts on unload", async () => {
    await setDraftAttachments("s-1", [img("SENT")]);
    localStorage.removeItem("acp:draft-attachments:s-1");
    expect(await storedIds()).toEqual(["s-1"]);
    expect(await getDraftAttachments("s-1")).toEqual([]);
  });

  it("does not warn about a failed older save after the draft was cleared", async () => {
    const errors = toastErrors();
    vi.spyOn(IDBObjectStore.prototype, "put").mockImplementationOnce(() => {
      throw new DOMException("The quota has been exceeded.", "QuotaExceededError");
    });
    const older = setDraftAttachments("s-1", [img("OLDER")]);
    const cleared = setDraftAttachments("s-1", []);
    await Promise.all([older, cleared]);
    expect(errors).toEqual([]);
    expect(await storedIds()).toEqual([]);
  });

  it("a delayed clear leaves a newer draft that another context saved meanwhile", async () => {
    const real = await vi.importActual<typeof import("./attachmentStore")>("./attachmentStore");
    await setDraftAttachments("s-1", [img("A")]);
    let release: () => void = () => {};
    saveGate.deleteHold = new Promise<void>((resolve) => (release = resolve));
    try {
      // The clear removes the marker now, but its delete transaction is delayed.
      const clearing = setDraftAttachments("s-1", []);
      // Another tab stages a newer draft before that delete runs: marker and record both land.
      localStorage.setItem("acp:draft-attachments:s-1", "1");
      await real.saveAttachments("s-1", [img("OTHER-TAB")], Date.now() + 10_000);
      release();
      await clearing;
      expect(await getDraftAttachments("s-1")).toEqual([img("OTHER-TAB")]);
    } finally {
      saveGate.deleteHold = null;
    }
  });

  it("re-arms the restore-failure toast after a successful restore", async () => {
    const errors = toastErrors();
    notifyAttachmentRestoreFailure("s-1");
    notifyAttachmentRestoreFailure("s-1");
    expect(errors).toHaveLength(1);
    await getDraftAttachments("s-1");
    expect(errors).toHaveLength(1);
    await setDraftAttachments("s-1", [img("X")]);
    await getDraftAttachments("s-1");
    notifyAttachmentRestoreFailure("s-1");
    expect(errors).toHaveLength(2);
  });

  it("restores only after writes already queued for the session have landed", async () => {
    let release: () => void = () => {};
    saveGate.hold = new Promise<void>((resolve) => (release = resolve));
    try {
      const first = setDraftAttachments("s-1", [img("A")]);
      const second = setDraftAttachments("s-1", [img("B")]);
      // Both saves wait on the gate, so this read is issued while A is in flight and B is queued.
      const restored = getDraftAttachments("s-1");
      release();
      await Promise.all([first, second]);
      expect(await restored).toEqual([img("B")]);
    } finally {
      saveGate.hold = null;
    }
  });

  it("applies overlapping writes in call order", async () => {
    const first = setDraftAttachments("s-1", [img("FIRST")]);
    const second = setDraftAttachments("s-1", [img("SECOND")]);
    await Promise.all([first, second]);
    expect(await getDraftAttachments("s-1")).toEqual([img("SECOND")]);
    const stage = setDraftAttachments("s-1", [img("THIRD")]);
    const clear = setDraftAttachments("s-1", []);
    await Promise.all([stage, clear]);
    expect([hasDraftAttachments("s-1"), await storedIds()]).toEqual([false, []]);
  });

  it("keeps a newer queued write when an older one fails", async () => {
    const errors = toastErrors();
    vi.spyOn(IDBObjectStore.prototype, "put").mockImplementationOnce(() => {
      throw new DOMException("The quota has been exceeded.", "QuotaExceededError");
    });
    const older = setDraftAttachments("s-1", [img("OLDER")]);
    const newer = setDraftAttachments("s-1", [img("NEWER")]);
    await Promise.all([older, newer]);
    expect(hasDraftAttachments("s-1")).toBe(true);
    expect(await getDraftAttachments("s-1")).toEqual([img("NEWER")]);
    expect(errors).toEqual([]);
  });

  it("removes the marker and record on a failed write and toasts once", async () => {
    const errors = toastErrors();
    await setDraftAttachments("s-1", [img("OLD")]);
    vi.spyOn(IDBObjectStore.prototype, "put").mockImplementation(() => {
      throw new DOMException("The quota has been exceeded.", "QuotaExceededError");
    });
    await setDraftAttachments("s-1", [img("NEW-TOO-BIG")]);
    await setDraftAttachments("s-1", [img("BIGGER")]);
    vi.restoreAllMocks();
    expect(localStorage.getItem("acp:draft-attachments:s-1")).toBeNull();
    expect(await storedIds()).toEqual([]);
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

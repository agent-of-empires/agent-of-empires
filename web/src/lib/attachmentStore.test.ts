// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import {
  clearAttachments,
  compareRevisions,
  deleteAttachments,
  loadAttachments,
  loadState,
  nextRevision,
  saveAttachments,
  type Revision,
} from "./attachmentStore";

describe("attachmentStore", () => {
  const att = (dataB64: string) => [{ kind: "image" as const, mimeType: "image/png", dataB64 }];

  it("a clear is a tombstone that older saves, from any tab, cannot overwrite", async () => {
    await saveAttachments("s-rev", att("OLD"), [100, "a"]);
    await saveAttachments("s-rev", att("NEWER"), [200, "a"]);
    await clearAttachments("s-rev", [150, "a"]);
    expect(await loadAttachments("s-rev")).toEqual(att("NEWER"));
    await clearAttachments("s-rev", [250, "a"]);
    expect(await loadAttachments("s-rev")).toEqual([]);
    expect(await loadState("s-rev")).toEqual({ rev: [250, "a"], saved: false });
    // A save issued before the clear arrives late: its own transaction sees the tombstone.
    await saveAttachments("s-rev", att("LATE"), [220, "b"]);
    expect(await loadState("s-rev")).toEqual({ rev: [250, "a"], saved: false });
    await saveAttachments("s-rev", att("AFTER"), [300, "a"]);
    expect(await loadState("s-rev")).toEqual({ rev: [300, "a"], saved: true });
    await deleteAttachments("s-rev");
    expect(await loadAttachments("s-rev")).toBeUndefined();
  });

  it("orders revisions antisymmetrically, including equal clocks", () => {
    const pairs: [Revision, Revision][] = [
      [
        [100, "a"],
        [100, "z"],
      ],
      [
        [100, "z"],
        [100, "a"],
      ],
      [
        [99, "z"],
        [100, "a"],
      ],
    ];
    for (const [a, b] of pairs) {
      const forward = Math.sign(compareRevisions(a, b));
      expect(forward).toBe(-Math.sign(compareRevisions(b, a)));
      expect(forward).not.toBeNaN();
    }
    const first = nextRevision();
    expect(compareRevisions(first, nextRevision())).toBeLessThan(0);
  });

  it("breaks equal clocks by tab id, so exactly one side is newer", async () => {
    await saveAttachments("s-tie", att("FROM-B"), [100, "b"]);
    await clearAttachments("s-tie", [100, "a"]);
    expect(await loadAttachments("s-tie")).toEqual(att("FROM-B"));
    await clearAttachments("s-tie", [100, "b"]);
    expect(await loadState("s-tie")).toEqual({ rev: [100, "b"], saved: false });
    await saveAttachments("s-tie", att("FROM-A"), [100, "a"]);
    expect(await loadState("s-tie")).toEqual({ rev: [100, "b"], saved: false });
    await saveAttachments("s-tie", att("FROM-B2"), [100, "c"]);
    await saveAttachments("s-tie", att("LATE-B"), [100, "b"]);
    expect(await loadAttachments("s-tie")).toEqual(att("FROM-B2"));
  });

  it("a queued older save cannot overwrite a newer one that landed first", async () => {
    await saveAttachments("s-a2", att("A1"), [100, "a"]);
    await saveAttachments("s-a2", att("B"), [300, "b"]);
    await saveAttachments("s-a2", att("A2"), [200, "a"]);
    expect(await loadAttachments("s-a2")).toEqual(att("B"));
  });

  it("reopens after another tab deletes the database", async () => {
    await saveAttachments("s-1", [{ kind: "image", mimeType: "image/png", dataB64: "AAAA" }]);
    // deleteDatabase fires versionchange on our open connection, which closes it.
    await new Promise<void>((resolve, reject) => {
      const req = indexedDB.deleteDatabase("aoe-composer-attachments-v2");
      req.onsuccess = () => resolve();
      req.onerror = () => reject(req.error);
    });
    await saveAttachments("s-2", [{ kind: "image", mimeType: "image/png", dataB64: "BBBB" }]);
    expect(await loadAttachments("s-1")).toBeUndefined();
    expect(await loadAttachments("s-2")).toEqual([{ kind: "image", mimeType: "image/png", dataB64: "BBBB" }]);
  });
});

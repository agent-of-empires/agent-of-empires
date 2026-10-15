// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import { deleteAttachments, loadAttachments, saveAttachments } from "./attachmentStore";

describe("attachmentStore", () => {
  it("a delete only removes a record that no newer write replaced", async () => {
    const att = (dataB64: string) => [{ kind: "image" as const, mimeType: "image/png", dataB64 }];
    await saveAttachments("s-rev", att("OLD"), 100);
    // Another context saved after the clear at 150 was issued: the delayed delete must leave it.
    await saveAttachments("s-rev", att("NEWER"), 200);
    await deleteAttachments("s-rev", 150);
    expect(await loadAttachments("s-rev")).toEqual(att("NEWER"));
    await deleteAttachments("s-rev", 200);
    expect(await loadAttachments("s-rev")).toBeUndefined();
    await saveAttachments("s-rev", att("ANY"), 300);
    await deleteAttachments("s-rev");
    expect(await loadAttachments("s-rev")).toBeUndefined();
  });

  it("reopens after another tab deletes the database", async () => {
    await saveAttachments("s-1", [{ kind: "image", mimeType: "image/png", dataB64: "AAAA" }]);
    // deleteDatabase fires versionchange on our open connection, which closes it.
    await new Promise<void>((resolve, reject) => {
      const req = indexedDB.deleteDatabase("aoe-composer-attachments");
      req.onsuccess = () => resolve();
      req.onerror = () => reject(req.error);
    });
    await saveAttachments("s-2", [{ kind: "image", mimeType: "image/png", dataB64: "BBBB" }]);
    expect(await loadAttachments("s-1")).toBeUndefined();
    expect(await loadAttachments("s-2")).toEqual([{ kind: "image", mimeType: "image/png", dataB64: "BBBB" }]);
  });
});

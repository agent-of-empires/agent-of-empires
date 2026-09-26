import { describe, it, expect } from "vitest";
import { processFile } from "@pierre/diffs";
import { changedLines } from "./changedLines";

// The exact patch format the Rust backend emits (libgit2
// `Patch::from_buffers(...).to_buf()`: git-style `diff --git` + `index`
// headers). This is the contract the client-side patch parsing depends on.
const OLD = "line 1\nline 2\nline 3\n";
const NEW = "line 1 modified\nline 2\nline 3\nnew line 4\n";
const PATCH = `diff --git a/test.txt b/test.txt
index 83db48f..f7c6dd6 100644
--- a/test.txt
+++ b/test.txt
@@ -1,3 +1,4 @@
-line 1
+line 1 modified
 line 2
 line 3
+new line 4
`;

describe("processFile on a similar-crate patch", () => {
  it("changedLines maps to file-accurate line numbers", () => {
    const meta = processFile(PATCH, {
      oldFile: { name: "test.txt", contents: OLD },
      newFile: { name: "test.txt", contents: NEW },
    })!;
    expect(changedLines(meta)).toEqual([
      { side: "old", lineNumber: 1, text: "line 1" },
      { side: "new", lineNumber: 1, text: "line 1 modified" },
      { side: "new", lineNumber: 4, text: "new line 4" },
    ]);
  });
});

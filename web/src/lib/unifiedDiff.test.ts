import { describe, expect, it } from "vitest";

import { diffPair } from "./diffPair";
import { unifiedDiff } from "./unifiedDiff";

describe("unifiedDiff", () => {
  it("prefixes each line with its change and heads the hunk with its path and range", () => {
    const { hunk } = diffPair("a\nb\nc\n", "a\nB\nc\n");
    expect(unifiedDiff("src/x.rs", hunk)).toBe(
      [
        "--- src/x.rs",
        "+++ src/x.rs",
        `@@ -${hunk.old_start},${hunk.old_lines} +${hunk.new_start},${hunk.new_lines} @@`,
        " a",
        "-b",
        "+B",
        " c",
      ].join("\n"),
    );
  });

  it("marks a new file as created from /dev/null", () => {
    const { hunk } = diffPair("", "one\ntwo\n");
    const lines = unifiedDiff("new.txt", hunk).split("\n");
    expect(lines.slice(0, 2)).toEqual(["--- /dev/null", "+++ new.txt"]);
    expect(lines.slice(3)).toEqual(["+one", "+two"]);
  });
});

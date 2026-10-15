import { test } from "node:test";
import assert from "node:assert/strict";
import { classifyKind } from "../src/toolKind.ts";

test("maps the tool palette and the unregistered task tool", () => {
  const cases: [string, string][] = [
    ["Read", "read"],
    ["Write", "edit"],
    ["Bash", "execute"],
    // Regression for #1904: `task` used to fall through to "other", which
    // rendered a generic card instead of the think card.
    ["task", "think"],
    // The model picks the casing, so every arm is matched case-insensitively.
    ["Task", "think"],
    ["TASK", "think"],
    ["bash", "execute"],
    // Anything the adapter does not know still falls through.
    ["WebFetch", "other"],
    ["", "other"],
  ];
  for (const [name, expected] of cases) {
    assert.equal(classifyKind(name), expected, name);
  }
});

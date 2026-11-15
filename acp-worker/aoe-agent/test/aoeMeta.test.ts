import { test } from "node:test";
import assert from "node:assert/strict";
import { toolCallMeta } from "../src/aoeMeta.ts";

test("a tool call carries a versioned, idempotency-keyed primitive", () => {
  assert.deepEqual(toolCallMeta("call_1", "Read"), {
    aoe: { v: 1, key: "tool-call:call_1", tool_presentation: { label: "Read" } },
  });
});

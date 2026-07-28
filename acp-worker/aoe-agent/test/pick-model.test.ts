import { test } from "node:test";
import assert from "node:assert/strict";
import { pickModel } from "../src/index.ts";

test("routes MiniMax- prefixed ids to the MiniMax provider", () => {
  const model = pickModel("MiniMax-M3");
  assert.ok(model, "pickModel should return a model for MiniMax-M3");
  assert.equal(
    (model as unknown as { provider: string }).provider,
    "anthropic.messages",
    "MiniMax models should use the Anthropic-compatible provider",
  );
});

test("routes minimax: prefixed ids and strips the prefix", () => {
  const model = pickModel("minimax:MiniMax-M2.7");
  assert.ok(model, "pickModel should return a model for minimax:MiniMax-M2.7");
  assert.equal(
    (model as unknown as { provider: string }).provider,
    "anthropic.messages",
    "minimax: prefix should route to the Anthropic-compatible provider",
  );
});

test("still routes other providers unchanged", () => {
  assert.ok(pickModel("claude-opus-4-7"));
  assert.ok(pickModel("gpt-4o"));
  assert.ok(pickModel("gemini-2.0-flash"));
});

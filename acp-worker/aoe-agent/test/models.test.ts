import assert from "node:assert/strict";
import test from "node:test";
import { pickModel } from "../src/models.ts";

test("routes Atlas Cloud models through the OpenAI-compatible provider", () => {
  const model = pickModel("atlas:deepseek-ai/deepseek-v4-pro");

  assert.equal(model.provider, "atlas-cloud.chat");
  assert.equal(model.modelId, "deepseek-ai/deepseek-v4-pro");
});

test("preserves existing provider routing", () => {
  assert.equal(pickModel("openai:gpt-5").provider, "openai.responses");
  assert.equal(
    pickModel("google:gemini-2.5-pro").provider,
    "google.generative-ai",
  );
  assert.equal(
    pickModel("anthropic:claude-sonnet-4-5").provider,
    "anthropic.messages",
  );
});

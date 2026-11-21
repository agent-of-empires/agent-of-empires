import { test } from "node:test";
import assert from "node:assert/strict";
import {
  DEFAULT_GATEWAY_BASE_URL,
  GATEWAY_PREFIX,
  gatewayBaseUrl,
  resolveModelRoute,
} from "../src/model.ts";

test("existing model ids keep routing to their original provider", () => {
  const cases: [string, string, string][] = [
    // Bare prefixes, untouched.
    ["claude-opus-4-7", "anthropic", "claude-opus-4-7"],
    ["gpt-5", "openai", "gpt-5"],
    ["gemini-2.5-pro", "google", "gemini-2.5-pro"],
    // Explicit tags, whose prefix is stripped.
    ["anthropic:claude-sonnet-4-5", "anthropic", "claude-sonnet-4-5"],
    ["openai:gpt-5-mini", "openai", "gpt-5-mini"],
    ["google:gemini-2.5-flash", "google", "gemini-2.5-flash"],
    // Unrecognised ids still fall back to Anthropic.
    ["some-local-model", "anthropic", "some-local-model"],
    ["vendor/model-without-prefix", "anthropic", "vendor/model-without-prefix"],
  ];
  for (const [modelId, provider, resolved] of cases) {
    assert.deepEqual(resolveModelRoute(modelId), { provider, modelId: resolved }, modelId);
  }
});

test("a gateway id keeps its vendor-namespaced model id intact", () => {
  assert.deepEqual(resolveModelRoute(`${GATEWAY_PREFIX}anthropic/claude-fable-5`), {
    provider: "openai-compatible",
    modelId: "anthropic/claude-fable-5",
  });
  assert.deepEqual(resolveModelRoute(`${GATEWAY_PREFIX}moonshotai/kimi-k3.1-pro`), {
    provider: "openai-compatible",
    modelId: "moonshotai/kimi-k3.1-pro",
  });
  // GPT-shaped gateway ids stay gateway ids: the prefix decides the provider.
  assert.deepEqual(resolveModelRoute(`${GATEWAY_PREFIX}gpt-5`), {
    provider: "openai-compatible",
    modelId: "gpt-5",
  });
});

test("a gateway id without a model is refused", () => {
  assert.throws(() => resolveModelRoute(GATEWAY_PREFIX), /needs a gateway model id/);
});

test("the gateway base URL defaults to Otari and honours the override", () => {
  assert.equal(gatewayBaseUrl({}), DEFAULT_GATEWAY_BASE_URL);
  // The /api prefix is required: Otari serves /api/v1, not /v1.
  assert.equal(DEFAULT_GATEWAY_BASE_URL, "https://api.otari.ai/api/v1");

  assert.equal(
    gatewayBaseUrl({ OTARI_BASE_URL: "http://gateway.internal:4000/v1" }),
    "http://gateway.internal:4000/v1",
  );
  // Trailing slashes would double up when the provider appends /chat/completions.
  assert.equal(
    gatewayBaseUrl({ OTARI_BASE_URL: "http://gateway.internal:4000/v1//" }),
    "http://gateway.internal:4000/v1",
  );
  // A blank override is not a base URL.
  assert.equal(gatewayBaseUrl({ OTARI_BASE_URL: "  " }), DEFAULT_GATEWAY_BASE_URL);
});

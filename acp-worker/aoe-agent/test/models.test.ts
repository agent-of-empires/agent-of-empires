import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ATLASCLOUD_DEFAULT_BASE_URL,
  ATLASCLOUD_DEFAULT_MODEL,
  atlasCloudApiKey,
  atlasCloudBaseURL,
  atlasCloudModelId,
} from "../src/models.ts";

test("resolves Atlas Cloud model aliases", () => {
  assert.equal(atlasCloudModelId("atlascloud"), ATLASCLOUD_DEFAULT_MODEL);
  assert.equal(atlasCloudModelId("atlas-cloud"), ATLASCLOUD_DEFAULT_MODEL);
  assert.equal(atlasCloudModelId("atlas"), ATLASCLOUD_DEFAULT_MODEL);
  assert.equal(
    atlasCloudModelId("atlascloud:deepseek-ai/deepseek-v4-pro"),
    "deepseek-ai/deepseek-v4-pro",
  );
  assert.equal(
    atlasCloudModelId("atlas-cloud:qwen/qwen3.5-flash"),
    "qwen/qwen3.5-flash",
  );
});

test("keeps existing provider model routes outside Atlas Cloud", () => {
  assert.equal(atlasCloudModelId("claude-opus-4-7"), null);
  assert.equal(atlasCloudModelId("openai:gpt-5"), null);
  assert.equal(atlasCloudModelId("google:gemini-2.5-pro"), null);
});

test("reads Atlas Cloud credential aliases", () => {
  assert.equal(
    atlasCloudApiKey({
      ATLASCLOUD_API_KEY: "primary",
      ATLAS_CLOUD_API_KEY: "secondary",
    }),
    "primary",
  );
  assert.equal(
    atlasCloudApiKey({ ATLAS_CLOUD_API_KEY: "secondary" }),
    "secondary",
  );
});

test("reads Atlas Cloud base URL aliases", () => {
  assert.equal(atlasCloudBaseURL({}), ATLASCLOUD_DEFAULT_BASE_URL);
  assert.equal(
    atlasCloudBaseURL({
      ATLASCLOUD_BASE_URL: "https://example.test/v1",
      ATLAS_CLOUD_BASE_URL: "https://fallback.test/v1",
    }),
    "https://example.test/v1",
  );
  assert.equal(
    atlasCloudBaseURL({ ATLAS_CLOUD_BASE_URL: "https://fallback.test/v1" }),
    "https://fallback.test/v1",
  );
});

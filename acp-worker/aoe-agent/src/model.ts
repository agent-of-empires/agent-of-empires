/**
 * Provider selection for `AOE_AGENT_MODEL`.
 *
 * `openai-compatible:<vendor>/<model>` addresses an OpenAI-compatible
 * gateway: the part after the prefix is the request's `model`, unchanged.
 * Every other id keeps the mapping it had before extraction (bare
 * `claude-`/`gpt-`/`gemini-` prefixes, `anthropic:`/`openai:`/`google:`
 * tags, and Anthropic for anything unrecognised), so no existing id starts
 * routing somewhere new.
 */

import { anthropic } from "@ai-sdk/anthropic";
import { google } from "@ai-sdk/google";
import { openai } from "@ai-sdk/openai";
import { createOpenAICompatible } from "@ai-sdk/openai-compatible";

export const DEFAULT_MODEL = "claude-opus-4-7";

/** `AOE_AGENT_MODEL` prefix that selects an OpenAI-compatible gateway. */
export const GATEWAY_PREFIX = "openai-compatible:";

/** Otari is the gateway AoE ships a default for; `OTARI_BASE_URL` overrides it. */
export const DEFAULT_GATEWAY_BASE_URL = "https://api.otari.ai/api/v1";

export type ModelRoute = {
  provider: "anthropic" | "openai" | "google" | "openai-compatible";
  modelId: string;
};

type Env = Record<string, string | undefined>;

const PREFIXED_PROVIDERS = [
  { bare: "claude-", tag: "anthropic:", provider: "anthropic" },
  { bare: "gpt-", tag: "openai:", provider: "openai" },
  { bare: "gemini-", tag: "google:", provider: "google" },
] as const;

export function resolveModelRoute(modelId: string): ModelRoute {
  if (modelId.startsWith(GATEWAY_PREFIX)) {
    const gatewayModelId = modelId.slice(GATEWAY_PREFIX.length);
    if (!gatewayModelId) {
      throw new Error(
        `AOE_AGENT_MODEL=${GATEWAY_PREFIX} needs a gateway model id, ` +
          `for example ${GATEWAY_PREFIX}anthropic/claude-fable-5`,
      );
    }
    return { provider: "openai-compatible", modelId: gatewayModelId };
  }
  for (const { bare, tag, provider } of PREFIXED_PROVIDERS) {
    if (modelId.startsWith(tag)) {
      return { provider, modelId: modelId.slice(tag.length) };
    }
    if (modelId.startsWith(bare)) {
      return { provider, modelId };
    }
  }
  return { provider: "anthropic", modelId };
}

/** Gateway endpoint: `OTARI_BASE_URL` when set, else Otari's OpenAI-compatible root. */
export function gatewayBaseUrl(env: Env = process.env): string {
  const override = env.OTARI_BASE_URL?.trim().replace(/\/+$/, "");
  return override ? override : DEFAULT_GATEWAY_BASE_URL;
}

export function pickModel(modelId: string, env: Env = process.env) {
  const route = resolveModelRoute(modelId);
  switch (route.provider) {
    case "anthropic":
      return anthropic(route.modelId);
    case "openai":
      return openai(route.modelId);
    case "google":
      return google(route.modelId);
    case "openai-compatible": {
      const apiKey = env.OTARI_API_KEY?.trim() || undefined;
      if (!apiKey) {
        process.stderr.write(
          `[aoe-agent] OTARI_API_KEY is not set; requests to ${gatewayBaseUrl(env)} will be unauthenticated\n`,
        );
      }
      return createOpenAICompatible({
        name: "openai-compatible",
        baseURL: gatewayBaseUrl(env),
        apiKey,
      })(route.modelId);
    }
  }
}

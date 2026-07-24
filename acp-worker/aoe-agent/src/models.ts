import { anthropic } from "@ai-sdk/anthropic";
import { google } from "@ai-sdk/google";
import { openai } from "@ai-sdk/openai";
import { createOpenAICompatible } from "@ai-sdk/openai-compatible";

export const ATLASCLOUD_DEFAULT_BASE_URL = "https://api.atlascloud.ai/v1";
export const ATLASCLOUD_DEFAULT_MODEL = "qwen/qwen3.5-flash";

const ATLASCLOUD_ALIASES = ["atlascloud", "atlas-cloud", "atlas"] as const;

type Env = Record<string, string | undefined>;

export function atlasCloudApiKey(env: Env = process.env): string | undefined {
  return env.ATLASCLOUD_API_KEY || env.ATLAS_CLOUD_API_KEY;
}

export function atlasCloudBaseURL(env: Env = process.env): string {
  return (
    env.ATLASCLOUD_BASE_URL ||
    env.ATLAS_CLOUD_BASE_URL ||
    ATLASCLOUD_DEFAULT_BASE_URL
  );
}

export function atlasCloudModelId(modelId: string): string | null {
  const trimmed = modelId.trim();
  for (const alias of ATLASCLOUD_ALIASES) {
    if (trimmed === alias) {
      return ATLASCLOUD_DEFAULT_MODEL;
    }
    const prefix = `${alias}:`;
    if (trimmed.startsWith(prefix)) {
      return trimmed.slice(prefix.length).trim() || ATLASCLOUD_DEFAULT_MODEL;
    }
  }
  return null;
}

export function pickModel(modelId: string) {
  const atlasModelId = atlasCloudModelId(modelId);
  if (atlasModelId) {
    const atlasCloud = createOpenAICompatible({
      name: "atlascloud",
      apiKey: atlasCloudApiKey(),
      baseURL: atlasCloudBaseURL(),
    });
    return atlasCloud(atlasModelId);
  }
  if (modelId.startsWith("claude-") || modelId.startsWith("anthropic:")) {
    return anthropic(modelId.replace(/^anthropic:/, ""));
  }
  if (modelId.startsWith("gpt-") || modelId.startsWith("openai:")) {
    return openai(modelId.replace(/^openai:/, ""));
  }
  if (modelId.startsWith("gemini-") || modelId.startsWith("google:")) {
    return google(modelId.replace(/^google:/, ""));
  }
  return anthropic(modelId);
}

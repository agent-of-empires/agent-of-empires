import { anthropic } from "@ai-sdk/anthropic";
import { google } from "@ai-sdk/google";
import { openai } from "@ai-sdk/openai";
import { createOpenAICompatible } from "@ai-sdk/openai-compatible";

const atlasCloud = createOpenAICompatible({
  name: "atlas-cloud",
  apiKey: process.env.ATLASCLOUD_API_KEY,
  baseURL: "https://api.atlascloud.ai/v1",
});

export function pickModel(modelId: string) {
  if (modelId.startsWith("atlas:")) {
    return atlasCloud.chatModel(modelId.replace(/^atlas:/, ""));
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

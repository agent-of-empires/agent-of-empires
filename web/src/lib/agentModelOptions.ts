export interface AgentModelOption {
  id: string;
  label: string;
}

export const CURSOR_MODEL_OPTIONS: AgentModelOption[] = [
  { id: "auto", label: "Auto" },
  { id: "composer-2.5-fast", label: "Composer 2.5 Fast" },
  { id: "composer-2.5", label: "Composer 2.5" },
  { id: "gpt-5.3-codex-low", label: "Codex 5.3 Low" },
  { id: "gpt-5.3-codex-low-fast", label: "Codex 5.3 Low Fast" },
  { id: "gpt-5.3-codex", label: "Codex 5.3" },
  { id: "gpt-5.3-codex-fast", label: "Codex 5.3 Fast" },
  { id: "gpt-5.3-codex-high", label: "Codex 5.3 High" },
  { id: "gpt-5.3-codex-high-fast", label: "Codex 5.3 High Fast" },
  { id: "gpt-5.3-codex-xhigh", label: "Codex 5.3 Extra High" },
  { id: "gpt-5.3-codex-xhigh-fast", label: "Codex 5.3 Extra High Fast" },
  { id: "gpt-5.2", label: "GPT-5.2" },
  { id: "gpt-5.2-fast", label: "GPT-5.2 Fast" },
  { id: "gpt-5.2-codex-low", label: "Codex 5.2 Low" },
  { id: "gpt-5.2-codex-low-fast", label: "Codex 5.2 Low Fast" },
  { id: "gpt-5.2-codex", label: "Codex 5.2" },
  { id: "gpt-5.2-codex-fast", label: "Codex 5.2 Fast" },
  { id: "gpt-5.2-codex-high", label: "Codex 5.2 High" },
  { id: "gpt-5.2-codex-high-fast", label: "Codex 5.2 High Fast" },
  { id: "gpt-5.2-codex-xhigh", label: "Codex 5.2 Extra High" },
  { id: "gpt-5.2-codex-xhigh-fast", label: "Codex 5.2 Extra High Fast" },
  { id: "gpt-5.1-codex-max-low", label: "Codex 5.1 Max Low" },
  { id: "gpt-5.1-codex-max-low-fast", label: "Codex 5.1 Max Low Fast" },
  { id: "gpt-5.1-codex-max-medium", label: "Codex 5.1 Max" },
  { id: "gpt-5.1-codex-max-medium-fast", label: "Codex 5.1 Max Medium Fast" },
  { id: "gpt-5.1-codex-max-high", label: "Codex 5.1 Max High" },
  { id: "gpt-5.1-codex-max-high-fast", label: "Codex 5.1 Max High Fast" },
  { id: "gpt-5.1-codex-max-xhigh", label: "Codex 5.1 Max Extra High" },
  {
    id: "gpt-5.1-codex-max-xhigh-fast",
    label: "Codex 5.1 Max Extra High Fast",
  },
  { id: "gpt-5.5-high", label: "GPT-5.5 1M High" },
  { id: "gpt-5.5-high-fast", label: "GPT-5.5 High Fast" },
  { id: "gpt-5.5-medium", label: "GPT-5.5 1M" },
  { id: "gpt-5.5-medium-fast", label: "GPT-5.5 Fast" },
  { id: "gpt-5.5-low", label: "GPT-5.5 1M Low" },
  { id: "gpt-5.5-low-fast", label: "GPT-5.5 Low Fast" },
  { id: "gpt-5.5-extra-high", label: "GPT-5.5 1M Extra High" },
  { id: "gpt-5.5-extra-high-fast", label: "GPT-5.5 Extra High Fast" },
  { id: "gpt-5.4-high", label: "GPT-5.4 1M High" },
  { id: "gpt-5.4-high-fast", label: "GPT-5.4 High Fast" },
  { id: "gpt-5.4-medium", label: "GPT-5.4 1M" },
  { id: "gpt-5.4-medium-fast", label: "GPT-5.4 Fast" },
  { id: "gpt-5.4-low", label: "GPT-5.4 1M Low" },
  { id: "gpt-5.4-xhigh", label: "GPT-5.4 1M Extra High" },
  { id: "gpt-5.4-xhigh-fast", label: "GPT-5.4 Extra High Fast" },
  { id: "gpt-5.4-mini-medium", label: "GPT-5.4 Mini" },
  { id: "gpt-5.4-mini-high", label: "GPT-5.4 Mini High" },
  { id: "gpt-5.4-mini-xhigh", label: "GPT-5.4 Mini Extra High" },
  { id: "gpt-5.1", label: "GPT-5.1" },
  { id: "gpt-5.1-high", label: "GPT-5.1 High" },
  { id: "gpt-5.1-low", label: "GPT-5.1 Low" },
  { id: "gpt-5-mini", label: "GPT-5 Mini" },
  { id: "claude-opus-4-8-thinking-high", label: "Opus 4.8 1M Thinking" },
  {
    id: "claude-opus-4-8-thinking-high-fast",
    label: "Opus 4.8 1M Thinking Fast",
  },
  { id: "claude-opus-4-8-high", label: "Opus 4.8 1M" },
  { id: "claude-opus-4-8-high-fast", label: "Opus 4.8 1M Fast" },
  { id: "claude-opus-4-8-xhigh", label: "Opus 4.8 1M Extra High" },
  { id: "claude-opus-4-8-xhigh-fast", label: "Opus 4.8 1M Extra High Fast" },
  { id: "claude-opus-4-7-thinking-high", label: "Opus 4.7 1M High Thinking" },
  {
    id: "claude-opus-4-7-thinking-high-fast",
    label: "Opus 4.7 1M High Thinking Fast",
  },
  { id: "claude-opus-4-7-high", label: "Opus 4.7 1M High" },
  { id: "claude-opus-4-7-high-fast", label: "Opus 4.7 1M High Fast" },
  { id: "claude-4.6-sonnet-medium", label: "Sonnet 4.6 1M" },
  { id: "claude-4.6-sonnet-medium-thinking", label: "Sonnet 4.6 1M Thinking" },
  { id: "claude-4.5-sonnet", label: "Sonnet 4.5" },
  { id: "claude-4.5-sonnet-thinking", label: "Sonnet 4.5 Thinking" },
  { id: "gemini-3.1-pro", label: "Gemini 3.1 Pro" },
  { id: "gemini-3-flash", label: "Gemini 3 Flash" },
  { id: "gemini-3.5-flash", label: "Gemini 3.5 Flash" },
  { id: "grok-4.3", label: "Grok 4.3 1M" },
  { id: "grok-build-0.1", label: "Grok Build 0.1 1M" },
  { id: "kimi-k2.5", label: "Kimi K2.5" },
];

export function agentModelOptions(agent: string): AgentModelOption[] {
  if (agent === "cursor") return CURSOR_MODEL_OPTIONS;
  return [];
}

export function agentBaseModelOptions(agent: string): AgentModelOption[] {
  const options = agentModelOptions(agent);
  if (agent !== "cursor") return options;

  const seen = new Map<string, AgentModelOption>();
  for (const option of options) {
    const baseId = stripFastVariant(option.id);
    if (!seen.has(baseId)) {
      seen.set(baseId, {
        id: baseId,
        label: stripFastLabel(option.label),
      });
    }
  }
  return [...seen.values()];
}

export function stripFastVariant(model: string): string {
  return model.endsWith("-fast") ? model.slice(0, -"-fast".length) : model;
}

export function stripFastLabel(label: string): string {
  return label.endsWith(" Fast") ? label.slice(0, -" Fast".length) : label;
}

export function modelSupportsFast(agent: string, model: string): boolean {
  if (agent !== "cursor") return false;
  const base = stripFastVariant(model);
  return CURSOR_MODEL_OPTIONS.some((option) => option.id === `${base}-fast`);
}

export function splitAgentModel(
  agent: string,
  model: string,
): { model: string; fast: boolean } {
  if (agent !== "cursor") return { model, fast: false };
  return {
    model: stripFastVariant(model),
    fast: model.endsWith("-fast"),
  };
}

export function composeAgentModel(
  agent: string,
  model: string,
  fast: boolean,
): string {
  const trimmed = model.trim();
  if (!trimmed) return "";
  if (agent === "cursor" && fast && modelSupportsFast(agent, trimmed)) {
    return `${stripFastVariant(trimmed)}-fast`;
  }
  return stripFastVariant(trimmed);
}

export function suppressAgentEffort(agent: string): boolean {
  return agent === "cursor";
}

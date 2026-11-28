import type { SuggestionProvider } from "./types";

/** The built-in provider: items an agent session pushes through the `suggest_session` tool or `aoe suggest`. */
export const AGENT_PROVIDER_ID = "builtin.agent";

export const isPluginProvider = (p: Pick<SuggestionProvider, "id">) => p.id !== AGENT_PROVIDER_ID;

/** A prompt to paste into any session so its agent pushes suggestions. */
export const AGENT_SUGGEST_PROMPT =
  "Look at this project and suggest up to three sessions worth starting next. " +
  "For each one, call the suggest_session tool (or run `aoe suggest <title> --prompt <prompt> --reason <why>`) " +
  "with a short title, the prompt that session should start with, and why it is worth doing now.";

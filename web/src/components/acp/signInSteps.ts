/** How an Agent signs in, shared by the startup error banner and the composer's sign-in popover. */
export interface SignInSteps {
  agentName: string;
  /** Run in a terminal where the Agent runs; null when aoe doesn't know the Agent's command. */
  command: string | null;
  /** The API key alternative, read from the environment that runs `aoe serve`. */
  apiKeyEnv: string | null;
}

export function signInSteps(agent: string | null | undefined, agentName: string, sandboxed: boolean): SignInSteps {
  if (agent === "claude" || agent === "claude-code") {
    return { agentName, command: "claude auth login", apiKeyEnv: "ANTHROPIC_API_KEY" };
  }
  if (agent === "codex") {
    // Codex's browser sign-in calls back to localhost, which a container can't receive.
    return { agentName, command: sandboxed ? "codex login --device-auth" : "codex login", apiKeyEnv: "OPENAI_API_KEY" };
  }
  return { agentName, command: null, apiKeyEnv: null };
}

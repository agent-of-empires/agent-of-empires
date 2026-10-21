const DISPLAY_NAMES: Record<string, string> = {
  claude: "Claude Code",
  codex: "Codex",
  opencode: "OpenCode",
  gemini: "Gemini CLI",
  cursor: "Cursor",
};

/** How copy names a built-in Agent; any other Agent by its registry name. */
export function agentDisplayName(agent: string | null | undefined): string {
  if (!agent) return "the Agent";
  return DISPLAY_NAMES[agent] ?? agent;
}

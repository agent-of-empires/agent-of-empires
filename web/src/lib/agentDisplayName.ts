// A Map, so custom registry keys such as `constructor` never hit Object.prototype.
const DISPLAY_NAMES = new Map([
  ["claude", "Claude Code"],
  ["codex", "Codex"],
  ["opencode", "OpenCode"],
  ["gemini", "Gemini CLI"],
  ["cursor", "Cursor"],
]);

/** How copy names a built-in Agent; any other Agent by its registry name. */
export function agentDisplayName(agent: string | null | undefined): string {
  if (!agent) return "the Agent";
  return DISPLAY_NAMES.get(agent) ?? agent;
}

import type { SuggestionItem, SuggestionPreset } from "./types";

/** "agent-of-empires · Claude Code · Structured view · worktree", what Start will run for the item. */
export function startSummary(
  item: SuggestionItem,
  preset: SuggestionPreset | undefined,
  agentName: (id: string) => string,
): string | undefined {
  if (!preset) return undefined;
  return [
    item.resolved_project?.name ?? "no Project yet",
    agentName(preset.agent),
    preset.view === "structured" ? "Structured view" : "Terminal view",
    preset.worktree ? "worktree" : null,
    preset.sandbox ? "sandbox" : null,
  ]
    .filter(Boolean)
    .join(" · ");
}

/** "owner/repo#12" as "#12". */
export const shortRef = (ref: string) => (ref.includes("#") ? ref.slice(ref.lastIndexOf("#")) : ref);

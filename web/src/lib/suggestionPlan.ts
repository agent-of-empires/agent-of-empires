import type { SuggestionItem, SuggestionPreset } from "./types";

/** "agent-of-empires · Claude Code · Structured view · worktree", what Start will run for the item. */
export function startSummary(
  item: SuggestionItem,
  preset: SuggestionPreset | undefined,
  agentName: (id: string) => string,
): string | undefined {
  const plan = item.start ?? preset;
  if (!plan) return undefined;
  return [
    (item.start ? item.start.project?.name : item.resolved_project?.name) ?? "no Project yet",
    agentName(plan.agent),
    plan.view === "structured" ? "Structured view" : "Terminal view",
    plan.worktree ? "worktree" : null,
    plan.sandbox ? "sandbox" : null,
  ]
    .filter(Boolean)
    .join(" · ");
}

/** "owner/repo#12" as "#12". */
export const shortRef = (ref: string) => (ref.includes("#") ? ref.slice(ref.lastIndexOf("#")) : ref);

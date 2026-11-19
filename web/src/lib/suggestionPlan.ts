import type { SuggestionItem } from "./types";

/** "agent-of-empires · Claude Code · Structured view · worktree", what Start will run for the item. */
export function startSummary(item: SuggestionItem, agentName: (id: string) => string): string | undefined {
  const plan = item.start;
  if (!plan) return undefined;
  return [
    plan.project ?? item.resolved_project?.name ?? "no Project yet",
    agentName(plan.agent),
    plan.view === "structured" ? "Structured view" : "Terminal view",
    plan.worktree ? "worktree" : null,
    plan.sandbox ? "sandbox" : null,
  ]
    .filter(Boolean)
    .join(" · ");
}

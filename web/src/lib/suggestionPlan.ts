import type { SessionResponse, StartedSession, SuggestionItem, SuggestionPreset } from "./types";

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

/** The snapshot's Session with its status and prompt error from the live sessions poll, which the snapshot lags. */
export function liveSession(started: StartedSession | undefined, sessions?: SessionResponse[]) {
  const live = started && sessions?.find((s) => s.id === started.id);
  return live
    ? {
        ...started,
        status: live.status,
        initial_prompt_error: live.initial_prompt_error,
        initial_prompt_error_code: live.initial_prompt_error_code,
      }
    : started;
}

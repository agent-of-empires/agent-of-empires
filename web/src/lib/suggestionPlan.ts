import { prepareSuggestion } from "./api";
import { OFFLINE_TITLE } from "./connectionState";
import { reportError, reportInfo } from "./toastBus";
import type {
  SessionResponse,
  StartedSession,
  SuggestionItem,
  SuggestionPreset,
  SuggestionPresetEntry,
  SuggestionsSnapshot,
} from "./types";
import type { StartMenuProps } from "../components/SuggestionStartMenu";

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

export const STALE_REASON = "No longer open at the source";

/** "Couldn't start: …" for a failed row; the server's reason may already end in a full stop. */
export const failureText = ({ reason, hint }: { reason: string; hint?: string }) =>
  `Couldn't start: ${/[.!?]$/.test(reason) ? reason : `${reason}.`}${hint ? ` ${hint}` : ""}`;

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

/** Why suggestion actions are off: CityHall or a read-only server first, then offline. */
export function lockTitle(readOnly: boolean, offline: boolean, cityhall = false) {
  if (readOnly) return cityhall ? "Off in CityHall mode" : "Read-only: can't create Sessions";
  return offline ? OFFLINE_TITLE : undefined;
}

/** The kind's preset prompt for this item, on the clipboard. */
export async function copySuggestionPrompt(item: Pick<SuggestionItem, "provider_id" | "id">) {
  const r = await prepareSuggestion(item.provider_id, item.id);
  if (!r.ok) return reportError(r.error);
  try {
    await navigator.clipboard.writeText(r.data.initial_prompt);
    reportInfo("Prompt copied");
  } catch {
    reportError("Could not copy the prompt.");
  }
}

/** The \u25be menu of a suggestion that is not started yet, shared by every view. */
export function startMenuFor(
  item: SuggestionItem,
  snapshot: SuggestionsSnapshot,
  opts: {
    startWithChanges: (item: SuggestionItem) => void;
    agentName: (id: string) => string;
    presetOf: (providerId: string, criterion: string) => SuggestionPresetEntry | undefined;
    onEditPreset?: (providerId: string, criterion: string) => void;
  },
): Omit<StartMenuProps, "disabled"> {
  const provider = snapshot.providers.find((p) => p.id === item.provider_id);
  const kindLabel = provider?.sources.find((s) => s.criterion === item.criterion)?.label;
  return {
    uses: startSummary(item, opts.presetOf(item.provider_id, item.criterion)?.preset, opts.agentName),
    providerName: provider?.name ?? item.provider_id,
    sourceUrl: item.source_url,
    kindLabel,
    onStartWithChanges: () => opts.startWithChanges(item),
    onCopyPrompt: () => void copySuggestionPrompt(item),
    onEditPreset:
      opts.onEditPreset && kindLabel ? () => opts.onEditPreset!(item.provider_id, item.criterion) : undefined,
  };
}

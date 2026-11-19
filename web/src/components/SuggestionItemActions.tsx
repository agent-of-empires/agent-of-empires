import { prepareSuggestion } from "../lib/api";
import { reportError, reportInfo } from "../lib/toastBus";
import { startSummary } from "../lib/suggestionPlan";
import { suggestionKey, type useSuggestions } from "../hooks/useSuggestions";
import type { SuggestionItem, SuggestionsSnapshot } from "../lib/types";
import { SuggestionActions } from "./SuggestionRow";

interface Props {
  item: SuggestionItem;
  snapshot: SuggestionsSnapshot;
  ctrl: ReturnType<typeof useSuggestions>;
  readOnly: boolean;
  agentName: (id: string) => string;
  onOpenSession: (sessionId: string) => void;
  onToggleDismiss: () => void;
  /** Opens Settings › Suggestions on this kind's preset; absent where Settings is out of reach. */
  onEditPreset?: (providerId: string, criterion: string) => void;
  /** A CityHall client cannot save the repo-to-Project link that "Start in…" makes. */
  canLink?: boolean;
}

/** Start, ▾, Start in… and the started row's menu for one suggestion. */
export function SuggestionItemActions({
  item,
  snapshot,
  ctrl,
  readOnly,
  agentName,
  onOpenSession,
  onToggleDismiss,
  onEditPreset,
  canLink = true,
}: Props) {
  const key = suggestionKey(item);
  const provider = snapshot.providers.find((p) => p.id === item.provider_id);
  const providerName = provider?.name ?? item.provider_id;
  const kindLabel = provider?.sources.find((s) => s.criterion === item.criterion)?.label;
  const linked = !!(item.resolved_project ?? item.start?.project);
  return (
    <SuggestionActions
      dismissed={item.dismissed}
      readOnly={readOnly}
      offline={ctrl.offline}
      busy={ctrl.busy !== null}
      starting={ctrl.starting.includes(key)}
      onStart={() => void ctrl.start(item)}
      stale={item.stale}
      failed={key in ctrl.failures}
      onToggleDismiss={onToggleDismiss}
      startIn={
        !linked && item.mapping_key
          ? {
              onPick: (path) => void ctrl.start(item, { project: { path } }),
              disabled: !canLink,
              title: canLink ? undefined : "This dashboard can't save the link to a Project",
            }
          : undefined
      }
      menu={
        item.dismissed
          ? undefined
          : {
              uses: startSummary(item, agentName),
              providerName,
              sourceUrl: item.source_url,
              kindLabel,
              onStartWithChanges: () => void ctrl.startWithChanges(item),
              onCopyPrompt: () => {
                void prepareSuggestion(item.provider_id, item.id).then(async (r) => {
                  if (!r.ok) return reportError(r.error);
                  try {
                    await navigator.clipboard.writeText(r.data.initial_prompt);
                    reportInfo("Prompt copied");
                  } catch {
                    reportError("Could not copy the prompt.");
                  }
                });
              },
              onEditPreset:
                onEditPreset && kindLabel ? () => onEditPreset(item.provider_id, item.criterion) : undefined,
            }
      }
      started={
        item.started_session && {
          onOpen: () => onOpenSession(item.started_session!.id),
          onStartAnother: () => void ctrl.startWithChanges(item, true),
          providerName,
          sourceUrl: item.source_url,
        }
      }
    />
  );
}

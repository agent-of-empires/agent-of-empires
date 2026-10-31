import { updateSuggestionSource } from "../lib/api";
import { suggestionKey, useDismissUndo, useSuggestions } from "../hooks/useSuggestions";
import { useIsWideViewport } from "../hooks/useIsWideViewport";
import {
  SUGGESTION_NOTE_CLASS,
  SuggestionActions,
  SuggestionRow,
  SuggestionStatusNotes,
  UndoBar,
} from "./SuggestionRow";
import type { WizardPrefill } from "./session-wizard/SessionWizard";

interface Props {
  readOnly?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
  onViewAll: () => void;
  onOpenSession: (sessionId: string) => void;
}

export function DashboardSuggestions({ readOnly = false, onPrepare, onViewAll, onOpenSession }: Props) {
  const wide = useIsWideViewport();
  const { snapshot, loadFailed, busy, refreshing, notice, failures, offline, act, refresh, start } =
    useSuggestions(onPrepare);
  const { undo, dismiss, clearUndo } = useDismissUndo(act);

  if (!snapshot || snapshot.providers.length === 0) return null;

  const active = snapshot.items.filter((item) => !item.dismissed);
  const rows = active.slice(0, wide ? 3 : 2);
  const nameOf = (id: string) => snapshot.providers.find((p) => p.id === id)?.name ?? id;
  const pending = snapshot.providers.filter((p) => p.status === "pending");
  const kindOf = (item: { provider_id: string; criterion: string }) =>
    snapshot.providers
      .find((p) => p.id === item.provider_id)
      ?.sources.find((src) => src.criterion === item.criterion)
      ?.label.toLowerCase();
  const alert = notice ?? (loadFailed ? "Could not load suggestions." : null);

  return (
    <section className="mb-4 mt-3 w-full max-w-2xl" aria-labelledby="suggested-sessions-heading">
      <div className="mb-2 flex items-baseline justify-between px-1">
        <h2
          id="suggested-sessions-heading"
          className="text-[11px] font-mono uppercase tracking-[0.16em] text-text-muted"
        >
          Suggested sessions
        </h2>
        <a
          href="/suggestions"
          onClick={(e) => {
            if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
            e.preventDefault();
            onViewAll();
          }}
          className="text-xs text-brand-500 hover:underline"
        >
          View all ({active.length})
        </a>
      </div>
      <ul className="overflow-hidden rounded-lg border border-surface-700/40 bg-surface-900">
        {rows.map((item) => (
          <SuggestionRow
            key={suggestionKey(item)}
            title={item.title}
            source={nameOf(item.provider_id)}
            reason={item.reason}
            project={item.project}
            session={item.started_session}
            stale={item.stale}
            failure={failures[suggestionKey(item)]}
            actions={
              <SuggestionActions
                readOnly={readOnly}
                offline={offline}
                busy={busy !== null}
                onStart={() => void start(item)}
                stale={item.stale}
                failed={suggestionKey(item) in failures}
                onToggleDismiss={() => void dismiss(item, true)}
                started={
                  item.started_session && {
                    onOpen: () => onOpenSession(item.started_session!.id),
                    onStartAnother: () => void start(item, true),
                    providerName: nameOf(item.provider_id),
                    sourceUrl: item.source_url,
                  }
                }
              />
            }
          />
        ))}
        {rows.length === 0 && pending.length > 0 && (
          <li className={SUGGESTION_NOTE_CLASS}>
            <span aria-hidden="true" className="font-mono text-brand-500">
              ⠙
            </span>{" "}
            Getting your first suggestions from {pending.map((p) => p.name).join(", ")}…
          </li>
        )}
        {rows.length === 0 && pending.length === 0 && (
          <li className={SUGGESTION_NOTE_CLASS}>
            Nothing to suggest right now.{" "}
            {!readOnly && !offline && (
              <button
                type="button"
                onClick={() => void refresh()}
                disabled={busy !== null || refreshing}
                className="cursor-pointer text-brand-500 hover:underline disabled:opacity-50"
              >
                Refresh
              </button>
            )}
          </li>
        )}
        <SuggestionStatusNotes readOnly={readOnly} offline={offline} />
      </ul>
      {alert && (
        <p role="alert" className="mt-2 px-1 text-xs text-status-error">
          {alert}
        </p>
      )}
      {undo && (
        <UndoBar
          title={undo.title}
          disabled={busy !== null || readOnly || offline}
          onUndo={() => void dismiss(undo, false)}
          stopLabel={kindOf(undo)}
          onStop={() => {
            void act(`stop:${suggestionKey(undo)}`, () =>
              updateSuggestionSource(undo.provider_id, undo.criterion, false),
            );
            clearUndo();
          }}
        />
      )}
    </section>
  );
}

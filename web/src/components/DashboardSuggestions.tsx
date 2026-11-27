import type { SessionResponse } from "../lib/types";
import { liveSession } from "../lib/suggestionPlan";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";
import { StartedPromptNote } from "./PromptBlockedNote";
import { useAgentNames } from "../hooks/useAgentNames";
import { useSuggestionPresets } from "../hooks/useSuggestionPresets";
import { SuggestionItemActions } from "./SuggestionItemActions";
import { SuggestionHooksReview } from "./SuggestionHooksReview";
import { suggestionKey, useDismissUndo, useSuggestions } from "../hooks/useSuggestions";
import { useIsWideViewport } from "../hooks/useIsWideViewport";
import {
  SUGGESTION_NOTE_CLASS,
  SuggestionRow,
  SuggestionStatusNotes,
  SuggestionsOffNote,
  UndoBar,
} from "./SuggestionRow";
import type { WizardPrefill } from "./session-wizard/SessionWizard";
import { BUTTON_INLINE } from "../lib/surfaces";

interface Props {
  readOnly?: boolean;
  /** Read-only because of CityHall mode, which the copy says. */
  cityhall?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
  onEditPreset?: (providerId: string, criterion: string) => void;
  canLink?: boolean;
  onViewAll: () => void;
  onOpenSession: (sessionId: string) => void;
  /** Live sessions, so a started row follows its Session's state. */
  sessions?: SessionResponse[];
}

export function DashboardSuggestions({
  readOnly = false,
  cityhall = false,
  onPrepare,
  onEditPreset,
  canLink = true,
  onViewAll,
  onOpenSession,
  sessions,
}: Props) {
  const wide = useIsWideViewport();
  const ctrl = useSuggestions(onPrepare, onOpenSession);
  const { snapshot, loadFailed, busy, refreshing, notice, refreshFailed, failures, offline, act, refresh } = ctrl;
  const agentName = useAgentNames();
  const presetOf = useSuggestionPresets();
  const { undo, dismiss, stop } = useDismissUndo(act);

  // Offline, the disconnect banner already explains a missing snapshot.
  if (!snapshot && loadFailed && !offline)
    return (
      <section className="mb-4 mt-3 w-full max-w-2xl" aria-label="Suggested sessions">
        <p role="alert" className="px-1 text-xs text-status-error">
          Could not load suggestions.{" "}
          <button type="button" onClick={() => void ctrl.load()} className={BUTTON_INLINE}>
            Try again
          </button>
        </p>
      </section>
    );
  if (!snapshot || snapshot.providers.length === 0 || !snapshot.prefs.homepage) return null;

  const active = snapshot.items.filter((item) => !item.dismissed);
  const rows = active.slice(0, wide ? 3 : 2);
  // A provider that is off is never asked, so it stays "pending" forever.
  const on = snapshot.providers.filter((p) => p.enabled);
  const pending = on.filter((p) => p.status === "pending");
  const errored = on.filter((p) => p.status === "error");
  const kindOf = (item: { provider_id: string; criterion: string }) =>
    snapshot.providers
      .find((p) => p.id === item.provider_id)
      ?.sources.find((src) => src.criterion === item.criterion)
      ?.label.toLowerCase();
  const alert = notice ?? (loadFailed ? "Could not load suggestions." : null);

  return (
    <section
      className="mb-4 mt-3 w-full max-w-2xl"
      aria-labelledby="suggested-sessions-heading"
      {...tourAnchor(TOUR_ANCHORS.dashboardSuggestions)}
    >
      <div className="mb-2 flex items-center justify-between px-1">
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
          className={BUTTON_INLINE}
        >
          {active.length > 0 ? `View all (${active.length})` : "View all"}
        </a>
      </div>
      <ul className="rounded-lg border border-surface-700/40 bg-surface-900">
        {rows.map((item) => (
          <SuggestionRow
            key={suggestionKey(item)}
            title={item.title}
            sourceRef={snapshot.prefs.show_source_ref ? item.source_ref : undefined}
            reason={item.reason}
            project={item.resolved_project?.name ?? item.project}
            session={liveSession(item.started_session, sessions)}
            stale={item.stale}
            failure={failures[suggestionKey(item)]}
            note={
              <StartedPromptNote
                session={liveSession(item.started_session, sessions)}
                onTryAgain={() => ctrl.retryBlocked(item)}
                disabled={readOnly || offline}
              />
            }
            actions={
              <SuggestionItemActions
                item={item}
                snapshot={snapshot}
                ctrl={ctrl}
                readOnly={readOnly}
                cityhall={cityhall}
                agentName={agentName}
                presetOf={presetOf}
                onOpenSession={onOpenSession}
                onToggleDismiss={() => void dismiss(item, true)}
                onEditPreset={onEditPreset}
                canLink={canLink}
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
        {errored.map((p) => (
          <li key={p.id} className={`${SUGGESTION_NOTE_CLASS} text-status-error`}>
            {p.name} · couldn&apos;t refresh, {p.error ?? "provider error"}
          </li>
        ))}
        {on.length === 0 && (
          <SuggestionsOffNote
            providers={snapshot.providers}
            readOnly={readOnly}
            disabled={busy !== null || offline}
            act={act}
          />
        )}
        {rows.length === 0 && pending.length === 0 && on.length > 0 && (
          <li className={SUGGESTION_NOTE_CLASS}>
            {errored.length === 0 && "Nothing to suggest right now. "}
            {!readOnly && !offline && (
              <button
                type="button"
                onClick={() => void refresh()}
                disabled={busy !== null || refreshing}
                className={BUTTON_INLINE}
              >
                Refresh
              </button>
            )}
          </li>
        )}
        <SuggestionStatusNotes readOnly={readOnly} offline={offline} cityhall={cityhall} homepage />
      </ul>
      {alert && (
        <p role="alert" className="mt-2 px-1 text-xs text-status-error">
          {alert}
          {(loadFailed || refreshFailed) && !readOnly && !offline && (
            <>
              {" "}
              <button
                type="button"
                onClick={() => void (loadFailed ? ctrl.load() : refresh())}
                disabled={busy !== null || refreshing}
                className={BUTTON_INLINE}
              >
                Try again
              </button>
            </>
          )}
        </p>
      )}
      <SuggestionHooksReview ctrl={ctrl} />
      {undo && (
        <UndoBar
          // A new dismissal gets a fresh bar, not the last one's Stop state.
          key={suggestionKey(undo)}
          title={undo.title}
          disabled={busy !== null || readOnly || offline}
          onUndo={() => void dismiss(undo, false)}
          stopLabel={kindOf(undo)}
          onStop={() => stop(undo)}
          onResume={() => stop(undo, true)}
        />
      )}
    </section>
  );
}

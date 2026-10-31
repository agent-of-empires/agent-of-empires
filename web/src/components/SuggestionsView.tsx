import { useState } from "react";
import { RefreshCw } from "lucide-react";
import { OFFLINE_TITLE } from "../lib/connectionState";
import { dismissSuggestion } from "../lib/api";
import { suggestionKey, useSuggestions } from "../hooks/useSuggestions";
import { SUGGESTION_NOTE_CLASS, SuggestionActions, SuggestionRow, SuggestionStatusNotes } from "./SuggestionRow";
import type { SessionResponse } from "../lib/types";
import type { WizardPrefill } from "./session-wizard/SessionWizard";

interface Props {
  readOnly?: boolean;
  /** Opens on the Dismissed filter. */
  initialDismissed?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
  /** Called when "Start without review" created the session directly. */
  onStarted?: (session: SessionResponse) => void;
}

export function SuggestionsView({ readOnly = false, initialDismissed = false, onPrepare, onStarted }: Props) {
  const { snapshot, loading, loadFailed, refreshing, busy, notice, offline, act, refresh, start } = useSuggestions(
    onPrepare,
    onStarted,
  );
  const [showDismissed, setShowDismissed] = useState(initialDismissed);

  const disabled = readOnly || offline;
  const unavailableTitle = readOnly ? "Read-only dashboard" : offline ? OFFLINE_TITLE : undefined;

  const items = snapshot?.items ?? [];
  const visible = items.filter((item) => item.dismissed === showDismissed);
  const dismissedCount = items.filter((item) => item.dismissed).length;

  return (
    <main className="flex-1 min-h-0 overflow-y-auto bg-surface-900 text-text-primary">
      <div className="mx-auto max-w-4xl p-4 md:p-8">
        <header className="flex flex-wrap items-start justify-between gap-4">
          <div>
            <h1 className="text-xl font-semibold">Suggested sessions</h1>
            <p className="mt-2 max-w-2xl text-sm text-text-secondary">
              Start opens the new session wizard with the draft prompt. Nothing runs until you press Launch.
            </p>
          </div>
          <button
            type="button"
            onClick={() => void refresh()}
            disabled={refreshing || disabled}
            title={unavailableTitle}
            className="inline-flex min-h-10 items-center gap-2 rounded-md border border-surface-700 px-3 text-sm text-text-secondary hover:bg-surface-800 disabled:opacity-50"
          >
            <RefreshCw className={`h-4 w-4 ${refreshing ? "animate-spin" : ""}`} />
            {refreshing ? "Refreshing…" : "Refresh"}
          </button>
        </header>

        {(notice || loadFailed) && (
          <div
            role="alert"
            className="mt-4 rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
          >
            {notice ?? "Could not load suggestions."}
          </div>
        )}

        {snapshot?.providers.map((provider) => (
          <section
            key={provider.id}
            aria-label={`Provider ${provider.name}`}
            className={`mt-4 rounded-md border p-3 text-sm text-text-secondary ${
              provider.status === "error" ? "border-status-warning/40 bg-status-warning/10" : "border-surface-700/50"
            }`}
          >
            <p>
              <strong>{provider.name}</strong>
              {provider.status === "pending" && " · waiting for first results"}
              {provider.status === "error" && ` · ${provider.error ?? "provider error"}`}
            </p>
          </section>
        ))}

        {(dismissedCount > 0 || showDismissed) && (
          <div className="mt-4 flex justify-end">
            <button
              type="button"
              aria-pressed={showDismissed}
              onClick={() => setShowDismissed((v) => !v)}
              className="min-h-9 rounded-md px-3 text-sm text-text-secondary hover:bg-surface-800"
            >
              {showDismissed ? "Show active" : `Dismissed (${dismissedCount})`}
            </button>
          </div>
        )}

        {loading && !snapshot && (
          <p role="status" className="py-10 text-center text-sm text-text-dim">
            Loading suggestions…
          </p>
        )}
        {snapshot && (
          <ul className="mt-4 overflow-hidden rounded-lg border border-surface-700/40 bg-surface-900">
            {visible.map((item) => {
              const key = suggestionKey(item);
              const providerName = snapshot.providers.find((p) => p.id === item.provider_id)?.name ?? item.provider_id;
              return (
                <SuggestionRow
                  key={key}
                  title={item.title}
                  source={providerName}
                  reason={item.reason}
                  project={item.project}
                  actions={
                    <SuggestionActions
                      dismissed={item.dismissed}
                      readOnly={readOnly}
                      offline={offline}
                      busy={busy !== null}
                      onStart={() => void start(item)}
                      onToggleDismiss={() =>
                        void act(key, () => dismissSuggestion(item.provider_id, item.id, !item.dismissed))
                      }
                    />
                  }
                />
              );
            })}
            {visible.length === 0 && (
              <li className={SUGGESTION_NOTE_CLASS}>
                {showDismissed
                  ? "No dismissed suggestions."
                  : snapshot.providers.length === 0
                    ? "No plugin provides suggestions."
                    : "Nothing to suggest right now."}
              </li>
            )}
            <SuggestionStatusNotes readOnly={readOnly} offline={offline} />
          </ul>
        )}
      </div>
    </main>
  );
}

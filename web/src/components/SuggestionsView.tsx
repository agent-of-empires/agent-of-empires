import { useState } from "react";
import { RefreshCw, Settings } from "lucide-react";
import { OFFLINE_TITLE } from "../lib/connectionState";
import { agoLabel } from "../lib/suggestionTime";
import type { SuggestionItem, SuggestionProvider } from "../lib/types";
import { suggestionKey, useDismissUndo, useSuggestions } from "../hooks/useSuggestions";
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
  /** Opens on the Dismissed filter. */
  initialDismissed?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
  onOpenSession: (sessionId: string) => void;
  onOpenSettings?: () => void;
}

type Filter = "all" | "dismissed" | { provider: string };

const CHIP = "inline-flex h-8 flex-none items-center gap-1.5 rounded-full border px-3 text-xs";

function providerStatus(provider: SuggestionProvider, itemCount: number) {
  if (provider.refreshing) return { text: `⠙ ${provider.name}: refreshing…`, tone: "text-text-muted" };
  if (provider.status === "pending")
    return { text: `${provider.name}: waiting for first results`, tone: "text-text-muted" };
  if (provider.status === "error")
    return {
      text: `${provider.name}: couldn't refresh, ${provider.error ?? "provider error"}`,
      tone: "text-status-error",
    };
  if (itemCount === 0) return { text: `${provider.name}: nothing new`, tone: "text-text-dim" };
  const updated = provider.updated_at ? `updated ${agoLabel(provider.updated_at * 1000)}` : "updated";
  return { text: `${provider.name}: ${updated}`, tone: "text-text-secondary" };
}

/** Items by the Project they resolve to, in first-seen order with unresolved last. */
function groupByProject(items: SuggestionItem[]) {
  const groups = new Map<string, { name: string; items: SuggestionItem[] }>();
  for (const item of items) {
    const key = item.resolved_project?.path ?? "";
    const group = groups.get(key) ?? { name: item.resolved_project?.name ?? "No project", items: [] };
    group.items.push(item);
    groups.set(key, group);
  }
  return [...groups.entries()]
    .sort(([a], [b]) => Number(a === "") - Number(b === ""))
    .map(([key, group]) => ({ key, ...group }));
}

export function SuggestionsView({
  readOnly = false,
  initialDismissed = false,
  onPrepare,
  onOpenSession,
  onOpenSettings,
}: Props) {
  const { snapshot, loading, loadFailed, refreshing, busy, notice, offline, act, refresh, start } =
    useSuggestions(onPrepare);
  const { undo, dismiss } = useDismissUndo(act);
  const [filter, setFilter] = useState<Filter>(initialDismissed ? "dismissed" : "all");

  const disabled = readOnly || offline;
  const unavailableTitle = readOnly ? "Read-only dashboard" : offline ? OFFLINE_TITLE : undefined;

  const items = snapshot?.items ?? [];
  const active = items.filter((item) => !item.dismissed);
  const dismissedCount = items.length - active.length;
  const showDismissed = filter === "dismissed";
  const visible =
    filter === "all"
      ? active
      : showDismissed
        ? items.filter((i) => i.dismissed)
        : active.filter((i) => i.provider_id === filter.provider);
  const providers = snapshot?.providers.filter((p) => p.enabled) ?? [];
  const anyRefreshing = refreshing || providers.some((p) => p.refreshing);
  const groups = groupByProject(visible);
  const headed = groups.some((g) => g.key !== "");
  const chip = (selected: boolean) =>
    `${CHIP} ${selected ? "border-brand-500 bg-brand-500/10 text-text-primary" : "border-surface-700 text-text-secondary hover:bg-surface-800"}`;

  return (
    <main className="flex-1 min-h-0 overflow-y-auto bg-surface-900 text-text-primary">
      <div className="mx-auto max-w-4xl p-4 md:p-8">
        <header className="flex items-start justify-between gap-4">
          <div>
            <h1 className="text-lg font-semibold">Suggested sessions</h1>
            <p className="mt-2 max-w-2xl text-sm text-text-secondary">
              Start opens the new session wizard with the draft prompt. Nothing runs until you press Launch.
            </p>
          </div>
          <div className="flex flex-none items-center gap-1">
            <button
              type="button"
              aria-label="Refresh"
              onClick={() => void refresh()}
              disabled={anyRefreshing || disabled}
              title={unavailableTitle}
              className="inline-flex h-8 items-center gap-2 rounded-md border border-surface-700 px-2.5 text-sm text-text-secondary hover:bg-surface-800 disabled:opacity-50 md:px-3"
            >
              <RefreshCw className={`h-4 w-4 ${anyRefreshing ? "animate-spin" : ""}`} />
              <span className="hidden md:inline">{anyRefreshing ? "Refreshing…" : "Refresh"}</span>
            </button>
            {onOpenSettings && (
              <button
                type="button"
                aria-label="Settings › Suggestions"
                title="Settings › Suggestions"
                onClick={onOpenSettings}
                className="inline-flex h-8 w-8 items-center justify-center rounded-md text-text-secondary hover:bg-surface-800"
              >
                <Settings className="h-4 w-4" />
              </button>
            )}
          </div>
        </header>

        {(notice || loadFailed) && (
          <div
            role="alert"
            className="mt-4 rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
          >
            {notice ?? "Could not load suggestions."}
          </div>
        )}

        {snapshot && (
          <>
            <div className="mt-4 flex gap-2 overflow-x-auto" role="group" aria-label="Filter suggestions">
              <button
                type="button"
                aria-pressed={filter === "all"}
                onClick={() => setFilter("all")}
                className={chip(filter === "all")}
              >
                All <span className="text-text-dim">{active.length}</span>
              </button>
              {providers.map((p) => {
                const selected = typeof filter === "object" && filter.provider === p.id;
                return (
                  <button
                    key={p.id}
                    type="button"
                    aria-pressed={selected}
                    onClick={() => setFilter({ provider: p.id })}
                    className={chip(selected)}
                  >
                    {p.name}{" "}
                    <span className="text-text-dim">{active.filter((i) => i.provider_id === p.id).length}</span>
                  </button>
                );
              })}
              {(dismissedCount > 0 || showDismissed) && (
                <button
                  type="button"
                  aria-pressed={showDismissed}
                  onClick={() => setFilter("dismissed")}
                  className={chip(showDismissed)}
                >
                  Dismissed <span className="text-text-dim">{dismissedCount}</span>
                </button>
              )}
            </div>
            <ul
              aria-label="Provider status"
              className="mt-3 flex flex-col gap-1 font-mono text-xs md:flex-row md:flex-wrap md:gap-x-[18px]"
            >
              {snapshot.providers
                .filter((p) => p.enabled)
                .map((provider) => {
                  const { text, tone } = providerStatus(
                    provider,
                    items.filter((i) => i.provider_id === provider.id).length,
                  );
                  return (
                    <li key={provider.id} className={tone}>
                      {text}
                    </li>
                  );
                })}
            </ul>
          </>
        )}

        {loading && !snapshot && (
          <p role="status" className="py-10 text-center text-sm text-text-dim">
            Loading suggestions…
          </p>
        )}
        {snapshot && (
          <>
            {groups.map((group) => (
              <section key={group.key} aria-label={headed ? `Project ${group.name}` : undefined}>
                {headed && (
                  <h2 className="mx-1 mb-1.5 mt-4 font-mono text-xs text-text-secondary">
                    {group.name} <span className="text-text-dim">{group.items.length}</span>
                  </h2>
                )}
                <ul
                  className={`overflow-hidden rounded-lg border border-surface-700/40 bg-surface-900 ${headed ? "" : "mt-4"}`}
                >
                  {group.items.map((item) => {
                    const key = suggestionKey(item);
                    const providerName =
                      snapshot.providers.find((p) => p.id === item.provider_id)?.name ?? item.provider_id;
                    return (
                      <SuggestionRow
                        key={key}
                        title={item.title}
                        source={providerName}
                        reason={item.reason}
                        project={item.project}
                        session={item.started_session}
                        actions={
                          <SuggestionActions
                            dismissed={item.dismissed}
                            readOnly={readOnly}
                            offline={offline}
                            busy={busy !== null}
                            onStart={() => void start(item)}
                            started={
                              item.started_session && {
                                onOpen: () => onOpenSession(item.started_session!.id),
                                onStartAnother: () => void start(item, true),
                                providerName,
                                sourceUrl: item.source_url,
                              }
                            }
                            onToggleDismiss={() => void dismiss(item, !item.dismissed)}
                          />
                        }
                      />
                    );
                  })}
                </ul>
              </section>
            ))}
            <ul className="mt-4 overflow-hidden rounded-lg border border-surface-700/40 bg-surface-900 empty:hidden">
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
            {undo && (
              <UndoBar
                title={undo.title}
                disabled={busy !== null || disabled}
                onUndo={() => void dismiss(undo, false)}
              />
            )}
          </>
        )}
      </div>
    </main>
  );
}

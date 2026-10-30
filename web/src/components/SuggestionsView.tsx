import { useCallback, useEffect, useRef, useState } from "react";
import { RefreshCw, Undo2, X } from "lucide-react";
import {
  dismissSuggestion,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  type SuggestionResult,
} from "../lib/api";
import type { SuggestionItem, SuggestionPrepared, SuggestionsSnapshot } from "../lib/types";
import { OFFLINE_TITLE, useServerDown } from "../lib/connectionState";
import type { WizardPrefill } from "./session-wizard/SessionWizard";

interface Props {
  readOnly?: boolean;
  /** Opens on the Dismissed filter. */
  initialDismissed?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
}

const STALE_NOTICE = "That suggestion changed. Refreshing the list.";

// `p.worktree` is dropped on purpose: the wizard resolves worktree itself, and
// `WizardPrefill.worktreeEnabled` means a registered-project override.
function toWizardPrefill(p: SuggestionPrepared, source: NonNullable<WizardPrefill["source"]>): WizardPrefill {
  return {
    source,
    title: p.title || undefined,
    path: p.project?.path,
    tool: p.tool,
    yoloMode: p.yolo,
    sandboxEnabled: p.sandbox,
    // A blank initial_prompt is rejected by POST /api/sessions, so send none.
    initialPrompt: p.initial_prompt || undefined,
  };
}

export function SuggestionsView({ readOnly = false, initialDismissed = false, onPrepare }: Props) {
  const offline = useServerDown();
  const [snapshot, setSnapshot] = useState<SuggestionsSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadFailed, setLoadFailed] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [showDismissed, setShowDismissed] = useState(initialDismissed);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const mounted = useRef(true);
  const loadSeq = useRef(0);

  const apply = useCallback((data: SuggestionsSnapshot | null, seq: number) => {
    if (!mounted.current || seq !== loadSeq.current) return;
    if (data) setSnapshot(data);
    setLoadFailed(!data);
    setLoading(false);
  }, []);

  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    apply(await fetchSuggestions(), seq);
  }, [apply]);

  useEffect(() => {
    mounted.current = true;
    void load();
    return () => {
      mounted.current = false;
    };
  }, [load]);

  const disabled = readOnly || offline;
  const startTitle = readOnly ? "Read-only: can't create Sessions" : offline ? OFFLINE_TITLE : undefined;
  const unavailableTitle = readOnly ? "Read-only dashboard" : offline ? OFFLINE_TITLE : undefined;

  // Runs a mutation, then reloads; a stale item gets a notice instead of a raw error.
  const act = async (key: string, run: () => Promise<SuggestionResult<unknown>>) => {
    setBusy(key);
    setNotice(null);
    const result = await run();
    if (!mounted.current) return;
    if (!result.ok) setNotice(result.stale ? STALE_NOTICE : result.error);
    await load();
    if (mounted.current) setBusy(null);
  };

  const refresh = async () => {
    setRefreshing(true);
    setNotice(null);
    const result = await refreshSuggestions();
    if (!mounted.current) return;
    if (!result.ok) setNotice(result.error);
    await load();
    if (mounted.current) setRefreshing(false);
  };

  const start = async (item: SuggestionItem, providerName: string) => {
    const key = `${item.provider_id}:${item.id}`;
    setBusy(key);
    setNotice(null);
    const result = await prepareSuggestion(item.provider_id, item.id);
    if (!mounted.current) return;
    if (result.ok) {
      setBusy(null);
      onPrepare(toWizardPrefill(result.data, { label: providerName, title: item.title, url: item.source_url }));
      return;
    }
    setNotice(result.stale ? STALE_NOTICE : result.error);
    await load();
    if (mounted.current) setBusy(null);
  };

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
              const key = `${item.provider_id}:${item.id}`;
              const providerName = snapshot.providers.find((p) => p.id === item.provider_id)?.name ?? item.provider_id;
              return (
                <li
                  key={key}
                  className="flex items-center gap-3 border-t border-surface-700/40 px-3 py-2.5 first:border-t-0"
                >
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-sm text-text-primary">{item.title}</p>
                    <p className="mt-0.5 truncate text-xs text-text-muted">
                      <span className="font-mono font-medium text-text-secondary">{providerName}</span>
                      {" · "}
                      {item.reason}
                      {item.project && (
                        <>
                          {" · "}
                          <span className="font-mono font-medium text-text-secondary">{item.project}</span>
                        </>
                      )}
                    </p>
                  </div>
                  <div className="flex flex-none gap-1">
                    {!item.dismissed && (
                      <button
                        type="button"
                        onClick={() => void start(item, providerName)}
                        disabled={busy !== null || disabled}
                        title={startTitle}
                        className="inline-flex h-8 items-center rounded-md border border-surface-700 bg-surface-800 px-3 text-xs font-medium text-text-primary hover:bg-surface-700/50 disabled:opacity-50"
                      >
                        Start
                      </button>
                    )}
                    {!readOnly && (
                      <button
                        type="button"
                        aria-label={item.dismissed ? "Restore" : "Dismiss"}
                        title={item.dismissed ? "Restore" : "Dismiss"}
                        onClick={() =>
                          void act(key, () => dismissSuggestion(item.provider_id, item.id, !item.dismissed))
                        }
                        disabled={busy !== null || disabled}
                        className="inline-flex h-8 w-8 items-center justify-center rounded-md text-text-secondary hover:bg-surface-700/50 disabled:opacity-50"
                      >
                        {item.dismissed ? <Undo2 className="h-4 w-4" /> : <X className="h-4 w-4" />}
                      </button>
                    )}
                  </div>
                </li>
              );
            })}
            {visible.length === 0 && (
              <li className="border-t border-surface-700/40 px-3 py-2 text-xs text-text-dim first:border-t-0">
                {showDismissed
                  ? "No dismissed suggestions."
                  : snapshot.providers.length === 0
                    ? "No plugin provides suggestions."
                    : "Nothing to suggest right now."}
              </li>
            )}
            {readOnly && (
              <li className="border-t border-surface-700/40 px-3 py-2 text-xs text-text-dim">
                This dashboard is read-only. Start, Dismiss and Refresh are off.
              </li>
            )}
            {offline && (
              <li className="border-t border-surface-700/40 px-3 py-2 text-xs text-text-dim">
                Disconnected. Suggestions show from the last fetch; reconnect to start one.
              </li>
            )}
          </ul>
        )}
      </div>
    </main>
  );
}

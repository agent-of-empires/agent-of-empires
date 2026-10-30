import { useCallback, useEffect, useRef, useState } from "react";
import { ArrowUpRight, RefreshCw, Sparkles } from "lucide-react";
import {
  dismissSuggestion,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  updateSuggestionSource,
  type SuggestionResult,
} from "../lib/api";
import type { SuggestionItem, SuggestionPrepared, SuggestionsSnapshot } from "../lib/types";
import { OFFLINE_TITLE, useServerDown } from "../lib/connectionState";
import type { WizardPrefill } from "./session-wizard/SessionWizard";

interface Props {
  readOnly?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
}

const STALE_NOTICE = "That suggestion changed. Refreshing the list.";
const isHttpUrl = (url: string) => /^https?:\/\//i.test(url);

// `p.worktree` is dropped on purpose: the wizard resolves worktree itself, and
// `WizardPrefill.worktreeEnabled` means a registered-project override.
function toWizardPrefill(p: SuggestionPrepared): WizardPrefill {
  return {
    path: p.project?.path,
    tool: p.tool,
    yoloMode: p.yolo,
    sandboxEnabled: p.sandbox,
    // A blank initial_prompt is rejected by POST /api/sessions, so send none.
    initialPrompt: p.initial_prompt || undefined,
  };
}

export function SuggestionsView({ readOnly = false, onPrepare }: Props) {
  const offline = useServerDown();
  const [snapshot, setSnapshot] = useState<SuggestionsSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadFailed, setLoadFailed] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [showDismissed, setShowDismissed] = useState(false);
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
    const seq = ++loadSeq.current;
    void fetchSuggestions().then((data) => apply(data, seq));
    return () => {
      mounted.current = false;
    };
  }, [apply]);

  const disabled = readOnly || offline;
  const unavailableTitle = readOnly ? "Read-only server" : offline ? OFFLINE_TITLE : undefined;

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

  const start = async (item: SuggestionItem) => {
    const key = `${item.provider_id}:${item.id}`;
    setBusy(key);
    setNotice(null);
    const result = await prepareSuggestion(item.provider_id, item.id);
    if (!mounted.current) return;
    if (result.ok) {
      setBusy(null);
      onPrepare(toWizardPrefill(result.data));
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
            {provider.sources.length > 0 && (
              <fieldset className="mt-2 grid gap-1 sm:grid-cols-2">
                <legend className="sr-only">{provider.name} sources</legend>
                {provider.sources.map((source) => (
                  <label
                    key={source.criterion}
                    className="flex items-start gap-2 text-xs"
                    title={source.description || undefined}
                  >
                    <input
                      type="checkbox"
                      checked={source.enabled}
                      disabled={disabled || busy !== null}
                      aria-label={`${provider.name}: ${source.label}`}
                      onChange={(e) =>
                        void act(`source:${provider.id}:${source.criterion}`, () =>
                          updateSuggestionSource(provider.id, source.criterion, e.target.checked),
                        )
                      }
                    />
                    <span>{source.label}</span>
                  </label>
                ))}
              </fieldset>
            )}
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
        {!loading && snapshot && visible.length === 0 && (
          <div className="mt-6 rounded-lg border border-dashed border-surface-700 p-8 text-center">
            <Sparkles className="mx-auto h-5 w-5 text-text-dim" />
            <h2 className="mt-3 text-sm font-medium">
              {showDismissed ? "No dismissed suggestions" : "No suggestions yet"}
            </h2>
            <p className="mt-1 text-sm text-text-dim">
              {snapshot.providers.length === 0
                ? "No plugin provides suggestions."
                : "Refresh to check the enabled sources."}
            </p>
          </div>
        )}

        <ul className="mt-4 space-y-3">
          {visible.map((item) => {
            const key = `${item.provider_id}:${item.id}`;
            const providerName = snapshot?.providers.find((p) => p.id === item.provider_id)?.name ?? item.provider_id;
            return (
              <li key={key} className="rounded-lg border border-surface-700/60 bg-surface-800/40 p-4">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <h3 className="text-sm font-semibold">{item.title}</h3>
                      <span className="rounded bg-surface-700/60 px-2 py-0.5 text-[11px] text-text-dim">
                        {providerName}
                      </span>
                    </div>
                    <p className="mt-1 text-sm text-text-secondary">{item.reason}</p>
                    {isHttpUrl(item.source_url) && (
                      <a
                        href={item.source_url}
                        target="_blank"
                        rel="noreferrer"
                        className="mt-2 inline-flex items-center gap-1 text-xs text-brand-500 hover:underline"
                      >
                        {item.source_ref} <ArrowUpRight className="h-3 w-3" />
                        <span className="sr-only"> (opens in new tab)</span>
                      </a>
                    )}
                    {item.preview && (
                      <figure
                        aria-label="Provider preview, untrusted"
                        className="mt-3 rounded-md border border-dashed border-status-warning/50 bg-surface-900/60 p-3"
                      >
                        <figcaption className="text-[11px] uppercase tracking-wide text-status-warning">
                          Provider text, untrusted
                        </figcaption>
                        <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap text-xs text-text-dim">
                          {item.preview}
                        </pre>
                      </figure>
                    )}
                  </div>
                  <div className="flex w-full flex-wrap gap-2 sm:w-auto sm:justify-end">
                    {!item.dismissed && (
                      <button
                        type="button"
                        onClick={() => void start(item)}
                        disabled={busy !== null || disabled}
                        title={unavailableTitle}
                        className="min-h-10 rounded-md bg-brand-600 px-3 text-sm font-medium text-white hover:bg-brand-500 disabled:opacity-50"
                      >
                        Start
                      </button>
                    )}
                    <button
                      type="button"
                      onClick={() => void act(key, () => dismissSuggestion(item.provider_id, item.id, !item.dismissed))}
                      disabled={busy !== null || disabled}
                      title={unavailableTitle}
                      className="min-h-10 rounded-md border border-surface-700 px-3 text-sm text-text-secondary hover:bg-surface-700/50 disabled:opacity-50"
                    >
                      {item.dismissed ? "Restore" : "Dismiss"}
                    </button>
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      </div>
    </main>
  );
}

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowUpRight, RefreshCw, Sparkles } from "lucide-react";
import {
  actOnSuggestion,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  updateSuggestionPolicy,
  updateSuggestionSource,
} from "../lib/api";
import type { SuggestionItem, SuggestionsSnapshot } from "../lib/types";
import { OFFLINE_TITLE, useServerDown } from "../lib/connectionState";
import type { WizardPrefill } from "./session-wizard/SessionWizard";

interface Props {
  profile?: string;
  readOnly?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
  onOpenSession: (sessionId: string) => void;
}
const EMPTY_ITEMS: SuggestionItem[] = [];

export function SuggestionsView(props: Props) {
  return <SuggestionsViewForProfile key={props.profile ?? ""} {...props} />;
}

function SuggestionsViewForProfile({ profile, readOnly = false, onPrepare, onOpenSession }: Props) {
  const offline = useServerDown();
  const [snapshot, setSnapshot] = useState<SuggestionsSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [showDismissed, setShowDismissed] = useState(false);
  const [projectFilter, setProjectFilter] = useState("all");
  const [sourceFilter, setSourceFilter] = useState("all");
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const current = useRef(true);
  const loadSequence = useRef(0);
  const policyBusyRef = useRef(false);
  const [policyBusy, setPolicyBusy] = useState(false);

  const load = useCallback(
    async (retainRefreshError = false) => {
      const sequence = ++loadSequence.current;
      const data = await fetchSuggestions(profile);
      if (!current.current || sequence !== loadSequence.current) return;
      if (data) {
        setSnapshot(data);
        if (!retainRefreshError) setError(null);
      } else {
        setError("Could not load suggestions. The last successful snapshot is still shown when available.");
      }
      setLoading(false);
    },
    [profile],
  );

  useEffect(() => {
    current.current = true;
    const sequence = ++loadSequence.current;
    void fetchSuggestions(profile).then((data) => {
      if (!current.current || sequence !== loadSequence.current) return;
      if (data) {
        setSnapshot(data);
        setError(null);
      } else {
        setError("Could not load suggestions. The last successful snapshot is still shown when available.");
      }
      setLoading(false);
    });
    return () => {
      current.current = false;
    };
  }, [profile]);

  const refresh = async () => {
    setRefreshing(true);
    setError(null);
    const accepted = await refreshSuggestions(profile);
    if (!current.current) return;
    if (!accepted) setError("Refresh failed. Showing the last successful snapshot; provider errors are listed below.");
    await load(!accepted);
    setRefreshing(false);
  };

  const items = snapshot?.items ?? EMPTY_ITEMS;
  const projects = useMemo(
    () => [...new Set(items.map((item) => item.project || item.project_path || "Unresolved project"))].sort(),
    [items],
  );
  const sources = useMemo(() => [...new Set(items.map((item) => item.provider_name))].sort(), [items]);
  const visibleItems = items.filter(
    (item) =>
      item.dismissed === showDismissed &&
      (projectFilter === "all" || (item.project || item.project_path || "Unresolved project") === projectFilter) &&
      (sourceFilter === "all" || item.provider_name === sourceFilter),
  );
  const groupedItems = useMemo(() => {
    const groups = new Map<string, SuggestionItem[]>();
    for (const item of visibleItems) {
      const key = item.project || item.project_path || "Unresolved project";
      groups.set(key, [...(groups.get(key) ?? []), item]);
    }
    return [...groups.entries()];
  }, [visibleItems]);

  const performAction = async (
    item: SuggestionItem,
    action: "dismiss" | "restore" | "start",
    createAnother = false,
  ) => {
    setBusyKey(`${item.provider_id}:${item.id}`);
    setActionError(null);
    const result = await actOnSuggestion(
      { provider_id: item.provider_id, id: item.id, revision: item.revision },
      action,
      profile,
      createAnother,
    );
    if (!current.current) return;
    setBusyKey(null);
    if (!result.ok) {
      setActionError(result.error || "Suggestion action failed; your snapshot was not changed.");
      return;
    }
    if (action === "start" && result.data?.session_id) {
      onOpenSession(result.data.session_id);
      return;
    }
    await load();
  };

  const customize = async (item: SuggestionItem) => {
    setBusyKey(`${item.provider_id}:${item.id}`);
    setActionError(null);
    const result = await prepareSuggestion(
      { provider_id: item.provider_id, id: item.id, revision: item.revision },
      profile,
    );
    if (!current.current) return;
    setBusyKey(null);
    if (!result.ok) {
      setActionError(result.error);
      return;
    }
    onPrepare({
      path: result.data.project_path,
      title: result.data.title,
      prompt: result.data.prompt,
      suggestion: result.data.suggestion,
      sourceRefs: [result.data.source_ref],
      profile,
    });
  };

  const changePolicy = async (enabled: boolean, rules: { provider_id: string; criterion: string }[]) => {
    if (!snapshot || readOnly || offline || policyBusyRef.current) return;
    policyBusyRef.current = true;
    setPolicyBusy(true);
    try {
      const ok = await updateSuggestionPolicy(enabled, rules, profile);
      if (!current.current) return;
      if (!ok) setActionError("Policy update failed; the existing policy remains in effect.");
      else await load();
    } finally {
      policyBusyRef.current = false;
      if (current.current) setPolicyBusy(false);
    }
  };

  const changeSource = async (providerId: string, criterion: string, enabled: boolean) => {
    if (!snapshot || readOnly || offline || policyBusyRef.current) return;
    policyBusyRef.current = true;
    setPolicyBusy(true);
    try {
      const ok = await updateSuggestionSource(providerId, criterion, enabled, profile);
      if (!current.current) return;
      if (!ok) setActionError("Source update failed; the previous switches remain in effect.");
      else await load();
    } finally {
      policyBusyRef.current = false;
      if (current.current) setPolicyBusy(false);
    }
  };

  const policyRules = snapshot?.auto_create.rules ?? [];
  const criteria = [
    ...new Map([
      ...policyRules.map(
        (rule) =>
          [`${rule.provider_id}\\u0000${rule.criterion}`, { ...rule, provider_name: rule.provider_id }] as const,
      ),
      ...items.map(
        (item) =>
          [
            `${item.provider_id}\\u0000${item.criterion}`,
            { provider_id: item.provider_id, provider_name: item.provider_name, criterion: item.criterion },
          ] as const,
      ),
    ]).values(),
  ];
  const unavailableTitle = readOnly ? "Read-only server" : offline ? OFFLINE_TITLE : undefined;

  return (
    <main className="flex-1 min-h-0 overflow-y-auto bg-surface-900 text-text-primary">
      <div className="mx-auto max-w-5xl p-4 md:p-8">
        <header className="flex flex-wrap items-start justify-between gap-4">
          <div>
            <p className="font-mono text-[11px] uppercase tracking-wider text-text-dim">Shared suggestions</p>
            <h1 className="mt-1 text-xl font-semibold">Suggested sessions</h1>
            <p className="mt-2 max-w-2xl text-sm text-text-secondary">
              Review source-backed work before starting a user-owned session. Source content is untrusted; automation
              never makes a session safe to run unattended.
            </p>
          </div>
          <button
            type="button"
            onClick={() => void refresh()}
            disabled={refreshing || readOnly || offline}
            title={unavailableTitle}
            className="inline-flex min-h-10 items-center gap-2 rounded-md border border-surface-700 px-3 text-sm text-text-secondary hover:bg-surface-800 disabled:opacity-50"
          >
            <RefreshCw className={`h-4 w-4 ${refreshing ? "animate-spin" : ""}`} />
            {refreshing ? "Refreshing…" : "Refresh"}
          </button>
        </header>

        {(error || actionError) && (
          <div
            role="alert"
            className="mt-4 rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
          >
            {actionError || error}
          </div>
        )}
        {snapshot?.providers.length ? (
          <section aria-label="Provider status" className="mt-4 space-y-2">
            {snapshot.providers.map((provider) => (
              <div
                key={provider.provider_id}
                className={`rounded-md border p-3 text-sm text-text-secondary ${
                  provider.error || provider.status === "error" || provider.status === "stale"
                    ? "border-status-warning/40 bg-status-warning/10"
                    : "border-surface-700/50"
                }`}
              >
                <p>
                  <strong>{provider.provider_name}:</strong> {provider.status}
                  {provider.error ? ` · ${provider.error}` : null}
                  {provider.updated_at ? ` · Updated ${provider.updated_at}` : null}
                  {provider.error || provider.status === "error" || provider.status === "stale"
                    ? " · Previously collected items remain available."
                    : null}
                </p>
                {provider.sources?.length ? (
                  <fieldset className="mt-2 grid gap-1 sm:grid-cols-2">
                    <legend className="sr-only">{provider.provider_name} sources</legend>
                    {provider.sources.map((source) => (
                      <label
                        key={`${provider.provider_id}:${source.criterion}`}
                        className="flex items-start gap-2 text-xs"
                        title={source.description || undefined}
                      >
                        <input
                          type="checkbox"
                          checked={source.enabled}
                          disabled={readOnly || offline || policyBusy}
                          aria-label={`${provider.provider_name}: ${source.label}`}
                          onChange={(event) =>
                            void changeSource(provider.provider_id, source.criterion, event.target.checked)
                          }
                        />
                        <span>{source.label}</span>
                      </label>
                    ))}
                  </fieldset>
                ) : null}
              </div>
            ))}
          </section>
        ) : null}

        <section
          className="mt-6 rounded-lg border border-surface-700/60 bg-surface-800/50 p-4"
          aria-labelledby="suggestion-policy-title"
        >
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div>
              <h2 id="suggestion-policy-title" className="text-sm font-medium">
                Automation policy
              </h2>
              <p className="mt-1 max-w-2xl text-xs text-text-dim">
                Off by default. Rules only select a reviewed provider and criterion; normal project, agent, permission,
                and host safety gates still apply. This is not unattended approval.
              </p>
            </div>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={snapshot?.auto_create.enabled ?? false}
                disabled={!snapshot || readOnly || offline || policyBusy}
                onChange={(event) => void changePolicy(event.target.checked, policyRules)}
              />
              Enable eligible rules
            </label>
          </div>
          {criteria.length > 0 && (
            <fieldset
              className="mt-3 grid gap-2 sm:grid-cols-2"
              disabled={!snapshot || readOnly || offline || policyBusy}
            >
              <legend className="sr-only">Reviewed providers and criteria</legend>
              {criteria.map((item) => {
                const checked = policyRules.some(
                  (rule) => rule.provider_id === item.provider_id && rule.criterion === item.criterion,
                );
                return (
                  <label
                    key={`${item.provider_id}:${item.criterion}`}
                    className="flex items-start gap-2 rounded-md border border-surface-700/50 p-2 text-xs"
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      title={unavailableTitle}
                      onChange={(event) => {
                        const next = event.target.checked
                          ? [...policyRules, { provider_id: item.provider_id, criterion: item.criterion }]
                          : policyRules.filter(
                              (rule) => !(rule.provider_id === item.provider_id && rule.criterion === item.criterion),
                            );
                        void changePolicy(snapshot?.auto_create.enabled ?? false, next);
                      }}
                    />
                    <span>
                      <strong>{item.provider_name}</strong> · {item.criterion}
                    </span>
                  </label>
                );
              })}
            </fieldset>
          )}
          {snapshot?.auto_create.last_error && (
            <p role="status" className="mt-2 text-xs text-status-error">
              {snapshot.auto_create.last_error}
            </p>
          )}
        </section>

        <div className="mt-6 flex flex-wrap items-center gap-2">
          <label className="text-xs text-text-dim" htmlFor="suggestion-project-filter">
            Project
          </label>
          <select
            id="suggestion-project-filter"
            value={projectFilter}
            onChange={(event) => setProjectFilter(event.target.value)}
            className="min-h-9 max-w-full rounded-md border border-surface-700 bg-surface-800 px-2 text-sm"
          >
            <option value="all">All projects</option>
            {projects.map((project) => (
              <option key={project}>{project}</option>
            ))}
          </select>
          <label className="ml-2 text-xs text-text-dim" htmlFor="suggestion-source-filter">
            Source
          </label>
          <select
            id="suggestion-source-filter"
            value={sourceFilter}
            onChange={(event) => setSourceFilter(event.target.value)}
            className="min-h-9 max-w-full rounded-md border border-surface-700 bg-surface-800 px-2 text-sm"
          >
            <option value="all">All sources</option>
            {sources.map((source) => (
              <option key={source}>{source}</option>
            ))}
          </select>
          <button
            type="button"
            aria-pressed={showDismissed}
            onClick={() => setShowDismissed((value) => !value)}
            className="ml-auto min-h-9 rounded-md px-3 text-sm text-text-secondary hover:bg-surface-800"
          >
            {showDismissed ? "Show active" : "Dismissed"} ({items.filter((item) => item.dismissed).length})
          </button>
        </div>

        {loading && !snapshot ? (
          <p role="status" className="py-10 text-center text-sm text-text-dim">
            Loading suggestions…
          </p>
        ) : null}
        {!loading && snapshot && !visibleItems.length && (
          <div className="mt-6 rounded-lg border border-dashed border-surface-700 p-8 text-center">
            <Sparkles className="mx-auto h-5 w-5 text-text-dim" />
            <h2 className="mt-3 text-sm font-medium">
              {items.length ? "No suggestions in this view" : "No suggestions yet"}
            </h2>
            <p className="mt-1 text-sm text-text-dim">
              {items.length
                ? "Try another project or source filter, or switch between active and dismissed."
                : "Refresh to check connected providers. Suggestions stay here even when you have no sessions or a project needs resolving."}
            </p>
          </div>
        )}

        <div className="mt-4 space-y-6">
          {groupedItems.map(([project, group]) => (
            <section key={project} aria-label={`Project ${project}`}>
              <h2 className="mb-2 border-b border-surface-700/50 pb-2 font-mono text-xs text-text-dim">
                {project} <span className="text-text-muted">· {group.length}</span>
              </h2>
              <ul className="space-y-3">
                {group.map((item) => {
                  const busy = busyKey === `${item.provider_id}:${item.id}`;
                  const linked = item.session_id;
                  return (
                    <li
                      key={`${item.provider_id}:${item.id}`}
                      className="rounded-lg border border-surface-700/60 bg-surface-800/40 p-4"
                    >
                      <div className="flex flex-wrap items-start justify-between gap-3">
                        <div className="min-w-0 flex-1">
                          <div className="flex flex-wrap items-center gap-2">
                            <h3 className="text-sm font-semibold">{item.title}</h3>
                            <span className="rounded bg-surface-700/60 px-2 py-0.5 text-[11px] text-text-dim">
                              {item.provider_name}
                            </span>
                            {linked && (
                              <span className="rounded bg-status-running/10 px-2 py-0.5 text-[11px] text-status-running">
                                Session linked
                              </span>
                            )}
                            {!item.can_start && !linked && item.start_blocked_reason && (
                              <span className="text-xs text-status-warning">{item.start_blocked_reason}</span>
                            )}
                          </div>
                          <p className="mt-1 text-sm text-text-secondary">{item.reason}</p>
                          <p className="mt-2 text-xs text-text-dim">{item.preview}</p>
                          <details open className="mt-3">
                            <summary className="w-fit cursor-pointer text-xs text-brand-500 focus-visible:outline focus-visible:outline-2 focus-visible:outline-brand-500">
                              Actual draft prompt
                            </summary>
                            <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap rounded-md bg-surface-900 p-3 text-xs text-text-secondary">
                              {item.draft.prompt}
                            </pre>
                          </details>
                          <a
                            href={item.source_url}
                            target="_blank"
                            rel="noreferrer"
                            className="mt-3 inline-flex items-center gap-1 text-xs text-brand-500 hover:underline"
                          >
                            View source <ArrowUpRight className="h-3 w-3" />
                            <span className="sr-only"> (opens in new tab)</span>
                          </a>
                        </div>
                        <div className="flex w-full flex-wrap gap-2 sm:w-auto sm:justify-end">
                          {item.dismissed ? (
                            <button
                              type="button"
                              onClick={() => void performAction(item, "restore")}
                              disabled={busy || readOnly || offline}
                              title={unavailableTitle}
                              className="min-h-10 rounded-md border border-surface-700 px-3 text-sm disabled:opacity-50"
                            >
                              Restore
                            </button>
                          ) : linked ? (
                            <>
                              <button
                                type="button"
                                onClick={() => onOpenSession(linked)}
                                className="min-h-10 rounded-md bg-brand-600 px-3 text-sm font-medium text-white hover:bg-brand-500"
                              >
                                Open linked session
                              </button>
                              <button
                                type="button"
                                onClick={() => void performAction(item, "start", true)}
                                disabled={busy || !item.project_path || readOnly || offline}
                                title={unavailableTitle}
                                className="min-h-10 rounded-md border border-surface-700 px-3 text-sm disabled:opacity-50"
                              >
                                Create another
                              </button>
                            </>
                          ) : (
                            <>
                              <button
                                type="button"
                                onClick={() => void performAction(item, "start")}
                                disabled={busy || !item.can_start || readOnly || offline}
                                title={unavailableTitle || item.start_blocked_reason || undefined}
                                className="min-h-10 rounded-md bg-brand-600 px-3 text-sm font-medium text-white hover:bg-brand-500 disabled:opacity-50"
                              >
                                {busy ? "Starting…" : "Start"}
                              </button>
                              <button
                                type="button"
                                onClick={() => void customize(item)}
                                disabled={busy || !item.can_start || readOnly || offline}
                                title={unavailableTitle || item.start_blocked_reason || undefined}
                                className="min-h-10 rounded-md border border-surface-700 px-3 text-sm disabled:opacity-50"
                              >
                                Customize
                              </button>
                            </>
                          )}
                          {!item.dismissed && (
                            <button
                              type="button"
                              onClick={() => void performAction(item, "dismiss")}
                              disabled={busy || readOnly || offline}
                              title={unavailableTitle}
                              className="min-h-10 rounded-md px-3 text-sm text-text-dim hover:bg-surface-700/50 disabled:opacity-50"
                            >
                              Dismiss
                            </button>
                          )}
                        </div>
                      </div>
                    </li>
                  );
                })}
              </ul>
            </section>
          ))}
        </div>
        {snapshot?.providers.length ? (
          <p className="mt-6 text-xs text-text-dim">
            {snapshot.providers.length} suggestion provider{snapshot.providers.length === 1 ? "" : "s"} · shared across
            daemon clients
          </p>
        ) : null}
      </div>
    </main>
  );
}

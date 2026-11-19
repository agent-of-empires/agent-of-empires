import { useState } from "react";
import { ChevronDown, ChevronRight, RefreshCw, Search, Settings, X } from "lucide-react";
import { OFFLINE_TITLE } from "../lib/connectionState";
import { agoLabel } from "../lib/suggestionTime";
import { chipId, filterSuggestions, keyChipGroups, type KeyChip } from "../lib/suggestionFilter";
import type { SuggestionItem, SuggestionProvider } from "../lib/types";
import { suggestionKey, useDismissUndo, useSuggestions } from "../hooks/useSuggestions";
import { SuggestionFilterMenu, FilterMenuItem } from "./SuggestionFilterMenu";
import { useAgentNames } from "../hooks/useAgentNames";
import { useSuggestionPresets } from "../hooks/useSuggestionPresets";
import { SuggestionItemActions } from "./SuggestionItemActions";
import { SuggestionHooksReview } from "./SuggestionHooksReview";
import { SuggestionMappingWizard } from "./SuggestionMappingWizard";
import { SuggestionsSetupHint } from "./SuggestionsSetupHint";
import { SUGGESTION_NOTE_CLASS, SuggestionRow, SuggestionStatusNotes, UndoBar } from "./SuggestionRow";
import type { WizardPrefill } from "./session-wizard/SessionWizard";

interface Props {
  readOnly?: boolean;
  /** Opens on the Dismissed filter. */
  initialDismissed?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
  onOpenSession: (sessionId: string) => void;
  onOpenSettings?: () => void;
  /** Opens Settings › Suggestions on a kind's preset. */
  onEditPreset?: (providerId: string, criterion: string) => void;
  /** False where the repo-to-Project link cannot be saved (CityHall client mode). */
  canLink?: boolean;
  /** Set when the first-run hint can open the Marketplace (not read-only or CityHall). */
  onOpenMarketplace?: () => void;
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

/** The `#12` of a `owner/repo#12` reference, which the group header already names. */
const shortRef = (ref: string) => (ref.includes("#") ? ref.slice(ref.lastIndexOf("#")) : ref);

/** Items by repo (`mapping_key`) in first-seen order, items with no repo last. */
function groupByRepo(items: SuggestionItem[]) {
  const groups = new Map<string, { key: string; name: string; items: SuggestionItem[] }>();
  for (const item of items) {
    const key = item.mapping_key ?? "";
    const name = key === "" ? "Other" : key.slice(key.indexOf("/") + 1);
    const group = groups.get(key) ?? { key, name, items: [] };
    group.items.push(item);
    groups.set(key, group);
  }
  return [...groups.values()].sort((a, b) => Number(a.key === "") - Number(b.key === ""));
}

export function SuggestionsView({
  readOnly = false,
  initialDismissed = false,
  onPrepare,
  onOpenSession,
  onOpenSettings,
  onEditPreset,
  canLink = true,
  onOpenMarketplace,
}: Props) {
  const ctrl = useSuggestions(onPrepare, onOpenSession);
  const { snapshot, loading, loadFailed, refreshing, busy, notice, failures, offline, act, refresh, load } = ctrl;
  const agentName = useAgentNames();
  const presetOf = useSuggestionPresets();
  const { undo, dismiss } = useDismissUndo(act);
  const [filter, setFilter] = useState<Filter>(initialDismissed ? "dismissed" : "all");
  const [query, setQuery] = useState("");
  const [keyChips, setKeyChips] = useState<KeyChip[]>([]);
  const [mapping, setMapping] = useState(false);
  const [kinds, setKinds] = useState<string[]>([]);
  const [collapsed, setCollapsed] = useState<string[]>([]);

  // A source chip belongs to the list it was picked in, so it does not outlive a tab change.
  const changeFilter = (next: Filter) => {
    setFilter(next);
    setKeyChips([]);
    setKinds([]);
  };

  const disabled = readOnly || offline;
  const unavailableTitle = readOnly ? "Read-only dashboard" : offline ? OFFLINE_TITLE : undefined;

  const items = snapshot?.items ?? [];
  const active = items.filter((item) => !item.dismissed);
  const dismissedCount = items.length - active.length;
  const showDismissed = filter === "dismissed";
  const listed =
    filter === "all"
      ? active
      : showDismissed
        ? items.filter((i) => i.dismissed)
        : active.filter((i) => i.provider_id === filter.provider);
  const chipGroups = keyChipGroups(listed);
  const narrowed = query.trim() !== "" || keyChips.length > 0 || kinds.length > 0;
  const kindId = (i: { provider_id: string; criterion: string }) => `${i.provider_id}:${i.criterion}`;
  const visible = filterSuggestions(listed, query, keyChips).filter(
    (i) => kinds.length === 0 || kinds.includes(kindId(i)),
  );
  const toggleChip = (chip: KeyChip) =>
    setKeyChips((current) =>
      current.some((c) => chipId(c) === chipId(chip))
        ? current.filter((c) => chipId(c) !== chipId(chip))
        : [...current, chip],
    );
  const chipSelected = (chip: KeyChip) => keyChips.some((c) => chipId(c) === chipId(chip));
  const providers = snapshot?.providers.filter((p) => p.enabled) ?? [];
  const anyRefreshing = refreshing || providers.some((p) => p.refreshing);
  const groups = groupByRepo(visible);
  const kindOptions = providers.flatMap((p) =>
    p.sources
      .map((src) => ({ id: kindId({ provider_id: p.id, criterion: src.criterion }), label: src.label }))
      .filter((k) => listed.some((i) => kindId(i) === k.id)),
  );
  const toggleKind = (id: string) => setKinds((c) => (c.includes(id) ? c.filter((k) => k !== id) : [...c, id]));
  const toggleCollapsed = (key: string) =>
    setCollapsed((c) => (c.includes(key) ? c.filter((k) => k !== key) : [...c, key]));
  const repoLabel =
    keyChips.length === 0
      ? "All repos"
      : keyChips.length === 1
        ? "key" in keyChips[0]!
          ? keyChips[0].key.slice(keyChips[0].key.indexOf("/") + 1)
          : `${keyChips[0]!.org}/`
        : `${keyChips.length} repos`;
  const kindLabel =
    kinds.length === 0
      ? "All kinds"
      : kinds.length === 1
        ? (kindOptions.find((k) => k.id === kinds[0])?.label ?? "1 kind")
        : `${kinds.length} kinds`;

  const chip = (selected: boolean) =>
    `${CHIP} ${selected ? "border-brand-500 bg-brand-500/10 text-text-primary" : "border-surface-700 text-text-secondary hover:bg-surface-800"}`;

  const noProviderNote =
    !readOnly && onOpenMarketplace ? (
      <SuggestionsSetupHint
        ignoreDismissed
        noProvider
        onOpenMarketplace={onOpenMarketplace}
        onFinished={() => void load()}
        fallback="No plugin provides suggestions."
      />
    ) : (
      "No plugin provides suggestions."
    );

  return (
    <main className="flex-1 min-h-0 overflow-y-auto bg-surface-900 text-text-primary">
      <div className="mx-auto max-w-4xl p-4 md:p-8">
        <header className="flex items-center justify-between gap-4">
          <div className="min-w-0">
            <h1 className="text-lg font-semibold">Suggested sessions</h1>
            {snapshot && (
              <ul aria-label="Provider status" className="mt-1 flex flex-wrap gap-x-3 font-mono text-xs text-text-dim">
                <li>{active.length}</li>
                {providers.map((provider) => {
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
            )}
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
          <div className="mt-3 flex flex-col gap-2">
            <div className="flex flex-wrap items-center gap-2">
              <label className="relative min-w-[160px] flex-1">
                <Search className="pointer-events-none absolute left-2.5 top-2 h-4 w-4 text-text-dim" />
                <input
                  type="search"
                  aria-label="Search suggestions"
                  placeholder="Search"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  className="h-8 w-full rounded-md border border-surface-700 bg-surface-800 pl-8 pr-2 text-sm text-text-primary placeholder:text-text-dim"
                />
              </label>
              {chipGroups.length > 0 && (
                <SuggestionFilterMenu label={repoLabel} active={keyChips.length > 0}>
                  <FilterMenuItem
                    selected={keyChips.length === 0}
                    count={listed.length}
                    onClick={() => setKeyChips([])}
                  >
                    All repos
                  </FilterMenuItem>
                  {chipGroups.flatMap((group) => {
                    const keyItems = group.keys.map(({ key, label, count }) => (
                      <FilterMenuItem
                        key={`key:${key}`}
                        selected={chipSelected({ key })}
                        title={key}
                        indent={group.org !== null}
                        count={count}
                        onClick={() => toggleChip({ key })}
                      >
                        {label}
                      </FilterMenuItem>
                    ));
                    if (group.org === null) return keyItems;
                    const org = { org: group.org };
                    return [
                      <FilterMenuItem
                        key={`org:${group.org}`}
                        selected={chipSelected(org)}
                        title={`Everything under ${group.org}/`}
                        count={group.count}
                        onClick={() => toggleChip(org)}
                      >
                        {group.org} (org)
                      </FilterMenuItem>,
                      ...keyItems,
                    ];
                  })}
                </SuggestionFilterMenu>
              )}
              {kindOptions.length > 1 && (
                <SuggestionFilterMenu label={kindLabel} active={kinds.length > 0}>
                  <FilterMenuItem selected={kinds.length === 0} count={listed.length} onClick={() => setKinds([])}>
                    All kinds
                  </FilterMenuItem>
                  {kindOptions.map((k) => (
                    <FilterMenuItem
                      key={k.id}
                      selected={kinds.includes(k.id)}
                      count={listed.filter((i) => kindId(i) === k.id).length}
                      onClick={() => toggleKind(k.id)}
                    >
                      {k.label}
                    </FilterMenuItem>
                  ))}
                </SuggestionFilterMenu>
              )}
              {narrowed && (
                <button
                  type="button"
                  onClick={() => {
                    setQuery("");
                    setKeyChips([]);
                    setKinds([]);
                  }}
                  className="inline-flex h-8 flex-none items-center gap-1 rounded-md px-2.5 text-xs text-text-secondary hover:bg-surface-800"
                >
                  <X className="h-3.5 w-3.5" />
                  Clear filters
                </button>
              )}
            </div>
            {(providers.length > 1 || dismissedCount > 0 || showDismissed) && (
              <div className="flex gap-2 overflow-x-auto" role="group" aria-label="Filter suggestions">
                {providers.length > 1 && (
                  <>
                    <button
                      type="button"
                      aria-pressed={filter === "all"}
                      onClick={() => changeFilter("all")}
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
                          onClick={() => changeFilter({ provider: p.id })}
                          className={chip(selected)}
                        >
                          {p.name}{" "}
                          <span className="text-text-dim">{active.filter((i) => i.provider_id === p.id).length}</span>
                        </button>
                      );
                    })}
                  </>
                )}
                {(dismissedCount > 0 || showDismissed) && (
                  <button
                    type="button"
                    aria-pressed={showDismissed}
                    onClick={() => changeFilter(showDismissed ? "all" : "dismissed")}
                    className={chip(showDismissed)}
                  >
                    Dismissed <span className="text-text-dim">{dismissedCount}</span>
                  </button>
                )}
              </div>
            )}
          </div>
        )}

        {loading && !snapshot && (
          <p role="status" className="py-10 text-center text-sm text-text-dim">
            Loading suggestions…
          </p>
        )}
        {snapshot && (
          <>
            {groups.map((group) => {
              const closed = collapsed.includes(group.key);
              const unlinked = group.key !== "" && group.items.every((i) => !i.resolved_project);
              return (
                <section key={group.key} aria-label={`Repo ${group.name}`}>
                  <h2 className="mx-1 mb-1.5 mt-4 font-mono text-xs text-text-secondary">
                    <button
                      type="button"
                      aria-expanded={!closed}
                      title={group.key || undefined}
                      onClick={() => toggleCollapsed(group.key)}
                      className="inline-flex cursor-pointer items-center gap-1.5 hover:text-text-primary"
                    >
                      {closed ? <ChevronRight className="h-3.5 w-3.5" /> : <ChevronDown className="h-3.5 w-3.5" />}
                      {group.name} <span className="text-text-dim">{group.items.length}</span>
                      {unlinked && <span className="text-text-dim"> · no Project yet</span>}
                    </button>
                    {unlinked && !disabled && (
                      <button
                        type="button"
                        onClick={() => setMapping(true)}
                        className="ml-2 cursor-pointer font-sans text-brand-500 hover:underline"
                      >
                        Link a Project
                      </button>
                    )}
                  </h2>
                  {!closed && (
                    <ul className="rounded-lg border border-surface-700/40 bg-surface-900">
                      {group.items.map((item) => {
                        const key = suggestionKey(item);
                        return (
                          <SuggestionRow
                            key={key}
                            title={item.title}
                            inlineRef={snapshot.prefs.show_source_ref ? shortRef(item.source_ref) : ""}
                            reason={item.reason}
                            session={item.started_session}
                            stale={item.stale}
                            failure={failures[key]}
                            actions={
                              <SuggestionItemActions
                                item={item}
                                snapshot={snapshot}
                                ctrl={ctrl}
                                readOnly={readOnly}
                                agentName={agentName}
                                presetOf={presetOf}
                                onOpenSession={onOpenSession}
                                onToggleDismiss={() => void dismiss(item, !item.dismissed)}
                                onEditPreset={onEditPreset}
                                canLink={canLink}
                              />
                            }
                          />
                        );
                      })}
                    </ul>
                  )}
                </section>
              );
            })}
            <ul className="mt-4 rounded-lg border border-surface-700/40 bg-surface-900 empty:hidden">
              {visible.length === 0 && (
                <li className={SUGGESTION_NOTE_CLASS}>
                  {narrowed && listed.length > 0
                    ? "No suggestions match the filters."
                    : showDismissed
                      ? "No dismissed suggestions."
                      : snapshot.providers.length === 0
                        ? noProviderNote
                        : "Nothing to suggest right now."}
                </li>
              )}
              {snapshot.providers
                .filter((p) => !p.enabled)
                .map((p) => (
                  <li key={p.id} className={SUGGESTION_NOTE_CLASS}>
                    {p.name} suggestions are hidden because {p.name} is off in Settings › Suggestions.{" "}
                    {onOpenSettings && (
                      <button
                        type="button"
                        onClick={onOpenSettings}
                        className="cursor-pointer text-brand-500 hover:underline"
                      >
                        Turn it on
                      </button>
                    )}
                  </li>
                ))}
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
      <SuggestionHooksReview ctrl={ctrl} />
      {mapping && snapshot && (
        <SuggestionMappingWizard
          items={items}
          providers={snapshot.providers}
          disabled={disabled}
          onClose={() => setMapping(false)}
        />
      )}
    </main>
  );
}

import { useEffect, useRef, useState, type ReactNode } from "react";
import { FROM_OTHER_PEOPLE_NOTE, fromOtherPeople } from "../../lib/suggestionKinds";
import { DndContext, PointerSensor, closestCenter, useSensor, useSensors, type DragEndEvent } from "@dnd-kit/core";
import { SortableContext, arrayMove, useSortable, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { ArrowUp, GripVertical } from "lucide-react";
import {
  runSuggestionFix,
  updateSuggestionPrefs,
  updateSuggestionSource,
  type SuggestionPrefsPatch,
  type SuggestionResult,
} from "../../lib/api";
import type { SettingsFieldDescriptor, SuggestionProvider, SuggestionsSnapshot } from "../../lib/types";
import { OFFLINE_TITLE, useServerDown } from "../../lib/connectionState";
import { agoLabel } from "../../lib/suggestionTime";
import { useAgentNames } from "../../hooks/useAgentNames";
import { useSuggestionPresets } from "../../hooks/useSuggestionPresets";
import { SectionHeader, SuggestionSwitch } from "./suggestionsShared";
import { SuggestionLinkedRepos } from "./SuggestionLinkedRepos";
import { SuggestionPresetEditor, type PresetFocus } from "./SuggestionPresetEditor";
import { PluginSuggestionSettings } from "./PluginSettingsSections";

const REFRESH_OPTIONS = [5, 15, 30, 60, 120];

function Row({ title, description, children }: { title: ReactNode; description: string; children: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 py-3">
      <div className="min-w-0">
        <div className="text-sm text-text-primary">{title}</div>
        <div className="mt-0.5 text-xs text-text-dim">{description}</div>
      </div>
      {children}
    </div>
  );
}

const BUTTON =
  "inline-flex h-8 shrink-0 items-center rounded-md border border-surface-700 bg-surface-800 px-3 text-xs font-medium text-text-primary hover:bg-surface-700/50 disabled:opacity-50";

interface ProviderRowProps {
  provider: SuggestionProvider;
  disabled: boolean;
  /** Reordering waits for the previous patch, so a move never starts from a stale order. */
  reorderLocked: boolean;
  first: boolean;
  onMove: (id: string, delta: -1 | 1) => void;
  onEnabled: (id: string, enabled: boolean) => void;
  onFix: (id: string) => void;
}

function ProviderRow({ provider, disabled, reorderLocked, first, onMove, onEnabled, onFix }: ProviderRowProps) {
  const { setNodeRef, attributes, listeners, transform, transition } = useSortable({
    id: provider.id,
    disabled: reorderLocked,
  });
  const status =
    provider.status === "error" ? (
      <span className="text-status-error-text">Couldn&apos;t refresh: {provider.error ?? "provider error"}</span>
    ) : (
      `${provider.updated_at ? `Updated ${agoLabel(provider.updated_at * 1000)}` : provider.status === "pending" ? "Waiting for first results" : "Plugin"}${provider.uses_agent ? " · Uses your Agent's usage" : ""}`
    );
  return (
    <li
      ref={setNodeRef}
      style={{ transform: CSS.Transform.toString(transform), transition }}
      tabIndex={0}
      aria-label={`Source ${provider.name}`}
      onKeyDown={(e) => {
        if (!e.altKey || reorderLocked || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
        e.preventDefault();
        onMove(provider.id, e.key === "ArrowUp" ? -1 : 1);
      }}
      className="border-t border-surface-700/40 px-3 py-2.5 first:border-t-0 min-h-11"
    >
      <div className="flex items-center gap-3">
        <button
          type="button"
          aria-label={`Drag ${provider.name} to reorder`}
          disabled={reorderLocked}
          {...attributes}
          {...listeners}
          className="hidden h-8 w-6 shrink-0 cursor-grab items-center justify-center text-text-dim md:inline-flex"
        >
          <GripVertical className="h-4 w-4" />
        </button>
        <button
          type="button"
          aria-label={`Move ${provider.name} up`}
          disabled={reorderLocked || first}
          onClick={() => onMove(provider.id, -1)}
          className="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-text-secondary hover:bg-surface-700/50 disabled:opacity-30 md:hidden"
        >
          <ArrowUp className="h-4 w-4" />
        </button>
        <div className="min-w-0 flex-1">
          <div className="text-sm text-text-primary">{provider.name}</div>
          <div className="text-xs text-text-dim">{status}</div>
        </div>
        {provider.status === "error" && provider.fix && (
          <button
            type="button"
            aria-label={provider.fix.label}
            disabled={disabled}
            onClick={() => onFix(provider.id)}
            className={`${BUTTON} min-h-11 md:min-h-0`}
          >
            <span className="md:hidden">Fix</span>
            <span className="hidden md:inline">{provider.fix.label}</span>
          </button>
        )}
        <SuggestionSwitch
          checked={provider.enabled}
          disabled={disabled}
          label={`Show ${provider.name} suggestions`}
          onChange={(v) => onEnabled(provider.id, v)}
        />
      </div>
    </li>
  );
}

interface Props {
  snapshot: SuggestionsSnapshot | null;
  failed: boolean;
  reload: () => Promise<void>;
  readOnly: boolean;
  onOpenSuggestions?: (dismissed: boolean) => void;
  presetFocus?: PresetFocus;
  /** Each provider's `suggestion_*` plugin settings; omitted where plugin settings are not editable. */
  pluginSettings?: {
    schema: SettingsFieldDescriptor[];
    settings: Record<string, unknown> | null;
    onSaveField: (section: string, field: string, value: unknown) => Promise<boolean>;
    /** Shown instead of the fields while the schema loads or after it fails. */
    notice?: ReactNode;
  };
}

export function SuggestionsSettings({
  snapshot,
  failed,
  reload,
  readOnly,
  onOpenSuggestions,
  presetFocus,
  pluginSettings,
}: Props) {
  const offline = useServerDown();
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [view, setView] = useState<{ providerId: string; criterion: string } | "linked" | null>(
    presetFocus ? { providerId: presetFocus.providerId, criterion: presetFocus.criterion } : null,
  );
  const agentName = useAgentNames();
  const presetOf = useSuggestionPresets();
  const mounted = useRef(true);
  // Keyed on primitives: App rebuilds `presetFocus` every render, which must not reopen the view.
  const focusKey = presetFocus && `${presetFocus.providerId}/${presetFocus.criterion}`;
  const [seenFocus, setSeenFocus] = useState(focusKey);
  if (presetFocus && focusKey !== seenFocus) {
    setSeenFocus(focusKey);
    setView({ providerId: presetFocus.providerId, criterion: presetFocus.criterion });
  }
  const sensors = useSensors(useSensor(PointerSensor, { activationConstraint: { distance: 4 } }));

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const disabled = readOnly || offline;
  const act = async (run: () => Promise<SuggestionResult<unknown>>) => {
    setNotice(null);
    setBusy(true);
    const result = await run();
    if (mounted.current && !result.ok) setNotice(result.error);
    await reload();
    if (mounted.current) setBusy(false);
  };
  const patch = (p: SuggestionPrefsPatch) => act(() => updateSuggestionPrefs(p));

  const retry = (
    <button
      type="button"
      onClick={() => void reload()}
      className="rounded-md border border-surface-700 px-2 py-0.5 text-xs text-text-secondary hover:bg-surface-800"
    >
      Retry
    </button>
  );
  if (!snapshot) {
    return (
      <p role="status" className="flex items-center justify-center gap-3 py-10 text-sm text-text-dim">
        {failed ? "Could not load suggestions." : "Loading suggestions…"}
        {failed && retry}
      </p>
    );
  }
  if (view === "linked") {
    return <SuggestionLinkedRepos snapshot={snapshot} disabled={disabled} onBack={() => setView(null)} />;
  }
  if (view) {
    const provider = snapshot.providers.find((p) => p.id === view.providerId);
    const source = provider?.sources.find((src) => src.criterion === view.criterion);
    if (provider && source) {
      return (
        <SuggestionPresetEditor
          provider={provider}
          source={source}
          entry={presetOf(provider.id, source.criterion)}
          item={snapshot.items.find((i) => i.provider_id === provider.id && i.criterion === source.criterion)}
          disabled={disabled}
          onBack={() => (presetFocus?.returnToSuggestions ? onOpenSuggestions?.(false) : setView(null))}
          onSaved={reload}
        />
      );
    }
  }

  const { prefs, providers } = snapshot;
  const dismissed = snapshot.items.filter((i) => i.dismissed).length;
  const order = providers.map((p) => p.id);
  const move = (id: string, delta: -1 | 1) => {
    const from = order.indexOf(id);
    const to = from + delta;
    if (to < 0 || to >= order.length) return;
    void patch({ provider_order: arrayMove(order, from, to) });
  };
  const onDragEnd = ({ active, over }: DragEndEvent) => {
    if (!over || active.id === over.id) return;
    void patch({ provider_order: arrayMove(order, order.indexOf(String(active.id)), order.indexOf(String(over.id))) });
  };
  const refreshOptions = REFRESH_OPTIONS.includes(prefs.refresh_minutes)
    ? REFRESH_OPTIONS
    : [...REFRESH_OPTIONS, prefs.refresh_minutes].sort((a, b) => a - b);

  return (
    <div className="space-y-6" title={offline ? OFFLINE_TITLE : undefined}>
      {failed && (
        <div
          role="alert"
          className="flex items-center gap-3 rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
        >
          Could not reload suggestions. The values below may be out of date.
          {retry}
        </div>
      )}
      {notice && (
        <div
          role="alert"
          className="rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
        >
          {notice}
        </div>
      )}
      <div className="divide-y divide-surface-700/40">
        <Row title="Show on the homepage" description="Up to 3 suggestions under the status line.">
          <SuggestionSwitch
            checked={prefs.homepage}
            disabled={disabled}
            label="Show on the homepage"
            onChange={(v) => void patch({ homepage: v })}
          />
        </Row>
        <Row title="Show in the sidebar" description={'A "Suggested" Group above Trash.'}>
          <SuggestionSwitch
            checked={prefs.sidebar}
            disabled={disabled}
            label="Show in the sidebar"
            onChange={(v) => void patch({ sidebar: v })}
          />
        </Row>
        <Row title="Show where each one comes from" description="Like owner/repo#123.">
          <SuggestionSwitch
            checked={prefs.show_source_ref}
            disabled={disabled}
            label="Show where each one comes from"
            onChange={(v) => void patch({ show_source_ref: v })}
          />
        </Row>
      </div>

      <section>
        <SectionHeader>Sources</SectionHeader>
        <p className="mb-2 text-xs text-text-dim">Drag to set which source comes first, or Alt+↑/↓ on a focused row.</p>
        <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
          <SortableContext items={order} strategy={verticalListSortingStrategy}>
            <ul className="overflow-hidden rounded-lg border border-surface-700/40 bg-surface-900">
              {providers.map((provider, i) => (
                <ProviderRow
                  key={provider.id}
                  provider={provider}
                  disabled={disabled}
                  reorderLocked={disabled || busy}
                  first={i === 0}
                  onMove={move}
                  onEnabled={(id, enabled) => void patch({ provider_enabled: { [id]: enabled } })}
                  onFix={(id) => void act(() => runSuggestionFix(id))}
                />
              ))}
            </ul>
          </SortableContext>
        </DndContext>
      </section>

      {providers.map((provider) => (
        <section key={provider.id} aria-label={`${provider.name} kinds`}>
          <SectionHeader>{`${provider.name} · Kinds`}</SectionHeader>
          <ul className="overflow-hidden rounded-lg border border-surface-700/40 bg-surface-900">
            {provider.sources.map((source) => {
              const preset = presetOf(provider.id, source.criterion)?.preset;
              const summary = !source.enabled
                ? "Not suggested"
                : preset
                  ? [
                      agentName(preset.agent),
                      preset.view === "structured" ? "Structured view" : "Terminal view",
                      preset.worktree ? "worktree" : null,
                      preset.sandbox ? "sandbox" : null,
                    ]
                      .filter(Boolean)
                      .join(" · ")
                  : "";
              return (
                <li
                  key={source.criterion}
                  className="flex min-h-11 items-center gap-3 border-t border-surface-700/40 px-3 py-2.5 first:border-t-0"
                >
                  <div className="min-w-0 flex-1">
                    <div className="text-sm text-text-primary" title={source.description || undefined}>
                      {source.label}
                    </div>
                    <div className="truncate text-xs text-text-dim">{summary}</div>
                    {fromOtherPeople(source.criterion) && (
                      <div className="text-xs text-text-dim">{FROM_OTHER_PEOPLE_NOTE}</div>
                    )}
                  </div>
                  <button
                    type="button"
                    aria-label={`Edit ${source.label}`}
                    className={BUTTON}
                    onClick={() => setView({ providerId: provider.id, criterion: source.criterion })}
                  >
                    Edit
                  </button>
                  <SuggestionSwitch
                    checked={source.enabled}
                    disabled={disabled || !provider.enabled}
                    label={`Suggest ${source.label}`}
                    onChange={(v) => void act(() => updateSuggestionSource(provider.id, source.criterion, v))}
                  />
                </li>
              );
            })}
          </ul>
        </section>
      ))}

      {pluginSettings &&
        (pluginSettings.notice ? (
          <section aria-label="Source settings">{pluginSettings.notice}</section>
        ) : (
          <fieldset disabled={readOnly} className="m-0 min-w-0 space-y-6 border-0 p-0">
            {providers.map((provider) => (
              <PluginSuggestionSettings
                key={provider.id}
                pluginId={provider.id}
                title={`${provider.name} source settings`}
                schema={pluginSettings.schema}
                settings={pluginSettings.settings}
                onSaveField={pluginSettings.onSaveField}
              />
            ))}
          </fieldset>
        ))}

      <details className="group">
        <summary className="cursor-pointer font-mono text-[11px] uppercase tracking-[0.14em] text-text-muted">
          Advanced
        </summary>
        <div className="mt-2 divide-y divide-surface-700/40">
          <Row title="Check for new suggestions every" description="Some sources check less often.">
            <select
              aria-label="Check for new suggestions every"
              value={prefs.refresh_minutes}
              disabled={disabled}
              onChange={(e) => void patch({ refresh_minutes: Number(e.target.value) })}
              className="h-8 rounded-md border border-surface-700 bg-surface-800 px-2 text-xs text-text-primary disabled:opacity-50"
            >
              {refreshOptions.map((m) => (
                <option key={m} value={m}>
                  {m < 60 ? `${m} min` : `${m / 60} h`}
                </option>
              ))}
            </select>
          </Row>
          <Row title="Linked repos" description="Which Project each repo starts in.">
            <button type="button" className={BUTTON} onClick={() => setView("linked")}>
              Edit
            </button>
          </Row>
          <Row title="Dismissed suggestions" description={`${dismissed} hidden.`}>
            <button type="button" className={BUTTON} onClick={() => onOpenSuggestions?.(true)}>
              Show
            </button>
          </Row>
        </div>
      </details>
    </div>
  );
}

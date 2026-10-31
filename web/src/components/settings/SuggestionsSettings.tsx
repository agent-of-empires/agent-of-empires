import { useEffect, useRef, useState, type ReactNode } from "react";
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
import type { SuggestionProvider, SuggestionsSnapshot } from "../../lib/types";
import { OFFLINE_TITLE, useServerDown } from "../../lib/connectionState";
import { SectionHeader, SuggestionSwitch } from "./suggestionsShared";
import { SuggestionTemplates, type TemplateFocus } from "./SuggestionTemplates";

const REFRESH_OPTIONS = [5, 15, 30, 60, 120];

function ago(updatedAt: number): string {
  const minutes = Math.max(0, Math.round((Date.now() / 1000 - updatedAt) / 60));
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  return hours < 24 ? `${hours} h ago` : `${Math.round(hours / 24)} d ago`;
}

function Row({
  title,
  description,
  children,
  dim,
}: {
  title: ReactNode;
  description: string;
  children: ReactNode;
  dim?: boolean;
}) {
  return (
    <div className={`flex items-center justify-between gap-4 py-3 ${dim ? "opacity-60" : ""}`}>
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
  onSource: (providerId: string, criterion: string, enabled: boolean) => void;
  onFix: (id: string) => void;
}

function ProviderRow({
  provider,
  disabled,
  reorderLocked,
  first,
  onMove,
  onEnabled,
  onSource,
  onFix,
}: ProviderRowProps) {
  const { setNodeRef, attributes, listeners, transform, transition } = useSortable({
    id: provider.id,
    disabled: reorderLocked,
  });
  const status =
    provider.status === "error" ? (
      <span className="text-status-error-text">Couldn&apos;t refresh: {provider.error ?? "provider error"}</span>
    ) : (
      `Plugin${provider.updated_at ? ` · updated ${ago(provider.updated_at)}` : provider.status === "pending" ? " · waiting for first results" : ""}${provider.uses_agent ? " · uses an Agent to read the codebase, which uses your Agent's usage" : ""}`
    );
  return (
    <li
      ref={setNodeRef}
      style={{ transform: CSS.Transform.toString(transform), transition }}
      tabIndex={0}
      aria-label={`Provider ${provider.name}`}
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
      {provider.sources.length > 0 && (
        <fieldset
          disabled={disabled || !provider.enabled}
          className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 pl-9 text-xs text-text-secondary disabled:opacity-50"
        >
          <legend className="sr-only">{provider.name} sources</legend>
          <span className="text-text-dim">Show</span>
          {provider.sources.map((source) => (
            <label key={source.criterion} className="flex items-center gap-1.5" title={source.description || undefined}>
              <input
                type="checkbox"
                className="accent-brand-600"
                checked={source.enabled}
                aria-label={`${provider.name}: ${source.label}`}
                onChange={(e) => onSource(provider.id, source.criterion, e.target.checked)}
              />
              {source.label}
            </label>
          ))}
        </fieldset>
      )}
    </li>
  );
}

interface Props {
  snapshot: SuggestionsSnapshot | null;
  failed: boolean;
  reload: () => Promise<void>;
  readOnly: boolean;
  onOpenSuggestions?: (dismissed: boolean) => void;
  templateFocus?: TemplateFocus;
}

export function SuggestionsSettings({ snapshot, failed, reload, readOnly, onOpenSuggestions, templateFocus }: Props) {
  const offline = useServerDown();
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [templates, setTemplates] = useState(Boolean(templateFocus));
  const mounted = useRef(true);
  // Keyed on primitives: App rebuilds `templateFocus` every render, which must not reopen the view.
  const focusKey = templateFocus && `${templateFocus.providerId}/${templateFocus.criterion}`;
  const [seenFocus, setSeenFocus] = useState(focusKey);
  if (focusKey !== seenFocus) {
    setSeenFocus(focusKey);
    if (focusKey) setTemplates(true);
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

  if (!snapshot) {
    return (
      <p role="status" className="py-10 text-center text-sm text-text-dim">
        {failed ? "Could not load suggestions." : "Loading suggestions…"}
      </p>
    );
  }
  if (templates) {
    return (
      <SuggestionTemplates
        snapshot={snapshot}
        disabled={disabled}
        focus={templateFocus}
        onBack={() => setTemplates(false)}
        onReturn={templateFocus?.returnToSuggestions ? () => onOpenSuggestions?.(false) : undefined}
        onSource={(providerId, criterion, enabled) => act(() => updateSuggestionSource(providerId, criterion, enabled))}
      />
    );
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
      {notice && (
        <div
          role="alert"
          className="rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
        >
          {notice}
        </div>
      )}
      <div className="divide-y divide-surface-700/40">
        <Row
          title="Show suggestions on the homepage"
          description="Up to 3 items between the status line and the cards."
        >
          <SuggestionSwitch
            checked={prefs.homepage}
            disabled={disabled}
            label="Show suggestions on the homepage"
            onChange={(v) => void patch({ homepage: v })}
          />
        </Row>
        <Row
          title="Show in sidebar"
          description={
            'A collapsed "Suggested" Group at the bottom of the session sidebar, above Trash. Off by default.'
          }
        >
          <SuggestionSwitch
            checked={prefs.sidebar}
            disabled={disabled}
            label="Show in sidebar"
            onChange={(v) => void patch({ sidebar: v })}
          />
        </Row>
      </div>

      <section>
        <SectionHeader>Providers · drag to set order</SectionHeader>
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
                  onSource={(id, criterion, enabled) => void act(() => updateSuggestionSource(id, criterion, enabled))}
                  onFix={(id) => void act(() => runSuggestionFix(id))}
                />
              ))}
            </ul>
          </SortableContext>
        </DndContext>
      </section>

      <div className="divide-y divide-surface-700/40">
        <Row title="Refresh every" description="Providers can refresh less often if they need to.">
          <select
            aria-label="Refresh every"
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
        <Row title="Dismissed suggestions" description={`${dismissed} hidden for this Profile.`}>
          <button type="button" className={BUTTON} onClick={() => onOpenSuggestions?.(true)}>
            Show
          </button>
        </Row>
        <Row
          title="Prompt templates and Projects"
          description="What each kind of suggestion asks the Agent, and which Project it starts in."
        >
          <button type="button" className={BUTTON} onClick={() => setTemplates(true)}>
            Edit
          </button>
        </Row>
        <Row
          dim
          title={
            <>
              Start without review{" "}
              <span className="ml-1 rounded border border-brand-600 px-1 font-mono text-[10px] text-brand-400">
                Later
              </span>
            </>
          }
          description="When on, Start launches with the template and mapping, skipping the wizard."
        >
          <SuggestionSwitch checked={false} disabled label="Start without review" onChange={() => {}} />
        </Row>
      </div>
    </div>
  );
}

import { useCallback, useEffect, useRef, useState } from "react";
import { X } from "lucide-react";
import {
  deleteSuggestionMapping,
  fetchProjects,
  fetchSuggestionMappings,
  fetchSuggestionTemplates,
  putSuggestionMapping,
  putSuggestionTemplate,
  resetSuggestionTemplate,
  type SuggestionResult,
} from "../../lib/api";
import type {
  ProjectInfo,
  SuggestionItem,
  SuggestionMapping,
  SuggestionProvider,
  SuggestionSource,
  SuggestionTemplate,
  SuggestionsSnapshot,
} from "../../lib/types";
import { expandTemplate } from "../../lib/suggestionTemplate";
import { TemplateEditor } from "./TemplateEditor";
import { SectionHeader, SuggestionSwitch } from "./suggestionsShared";

const GHOST =
  "inline-flex h-8 items-center rounded-md px-3 text-xs font-medium text-text-secondary hover:bg-surface-800 disabled:opacity-50";

interface BlockProps {
  provider: SuggestionProvider;
  source: SuggestionSource;
  override: string | null;
  item: SuggestionItem | undefined;
  disabled: boolean;
  onSource: (providerId: string, criterion: string, enabled: boolean) => Promise<void>;
  onSaved: () => Promise<void>;
  onError: (message: string) => void;
}

function TemplateBlock({ provider, source, override, item, disabled, onSource, onSaved, onError }: BlockProps) {
  const [edited, setEdited] = useState<string | null>(null);
  const value = edited ?? override ?? "";
  const effective = edited ?? override;

  const run = async (request: Promise<SuggestionResult<unknown>>) => {
    const result = await request;
    if (!result.ok) onError(result.error);
    setEdited(null);
    await onSaved();
  };
  const commit = () => {
    if (edited !== null && edited !== override) void run(putSuggestionTemplate(provider.id, source.criterion, edited));
    else setEdited(null);
  };

  let preview: string;
  if (!item) preview = "no current suggestion to preview.";
  else if (effective === "") preview = "starts with an empty prompt, so you type the first message.";
  else preview = effective === null ? item.draft.prompt : expandTemplate(effective, item.variables ?? {});

  // Keeps acronyms: "CI failures" stays, "Review requests" becomes "review requests".
  const plural = /^[A-Z]{2}/.test(source.label)
    ? source.label
    : source.label.charAt(0).toLowerCase() + source.label.slice(1);
  return (
    <div className="rounded-lg border border-surface-700/40 bg-surface-900 p-3">
      <div className={`flex flex-wrap items-center gap-2 ${source.enabled ? "" : "opacity-60"}`}>
        <code className="font-mono text-xs text-text-secondary">{source.criterion}</code>
        {source.description && (
          <span className="rounded border border-surface-700 px-1.5 text-[11px] text-text-dim">
            {source.description}
          </span>
        )}
        <span className="flex-1" />
        <span className="text-xs text-text-dim">Show {plural}</span>
        <SuggestionSwitch
          checked={source.enabled}
          disabled={disabled}
          label={`Show ${plural}`}
          onChange={(v) => void onSource(provider.id, source.criterion, v)}
        />
        <button
          type="button"
          className={GHOST}
          disabled={disabled || override === null}
          onClick={() => void run(resetSuggestionTemplate(provider.id, source.criterion))}
        >
          Reset to the plugin&apos;s default
        </button>
      </div>
      {source.enabled ? (
        <div className="mt-2">
          <TemplateEditor
            label={`${provider.name} ${source.criterion} template`}
            value={value}
            variables={source.variables}
            disabled={disabled}
            onChange={setEdited}
            onCommit={commit}
          />
          {effective === null && <p className="mt-2 text-xs text-text-dim">Uses the plugin&apos;s own prompt.</p>}
          <p className="mt-2 break-words font-mono text-xs text-text-muted">Preview: {preview}</p>
        </div>
      ) : (
        <p className="mt-2 text-xs text-text-dim">
          {source.label} from {provider.name} won&apos;t be suggested.
        </p>
      )}
    </div>
  );
}

interface MappingRowsProps {
  providers: SuggestionProvider[];
  items: SuggestionItem[];
  mappings: SuggestionMapping[];
  projects: ProjectInfo[];
  disabled: boolean;
  onChange: (request: Promise<SuggestionResult<unknown>>) => Promise<void>;
}

function MappingRows({ providers, items, mappings, projects, disabled, onChange }: MappingRowsProps) {
  const rows = new Map<string, { providerId: string; key: string; projects: string[] }>();
  for (const m of mappings)
    rows.set(`${m.provider_id}\0${m.mapping_key}`, {
      providerId: m.provider_id,
      key: m.mapping_key,
      projects: m.projects,
    });
  for (const item of items) {
    if (!item.mapping_key) continue;
    const id = `${item.provider_id}\0${item.mapping_key}`;
    if (!rows.has(id)) rows.set(id, { providerId: item.provider_id, key: item.mapping_key, projects: [] });
  }
  const name = (path: string) => projects.find((p) => p.path === path)?.name ?? path;
  const list = [...rows.values()].sort((a, b) => a.key.localeCompare(b.key));
  const multi = providers.length > 1;

  if (list.length === 0) return <p className="px-3 py-2 text-xs text-text-dim">No sources to map yet.</p>;
  return (
    <ul className="overflow-hidden rounded-lg border border-surface-700/40 bg-surface-900">
      {list.map((row) => {
        const free = projects.filter((p) => !row.projects.includes(p.path));
        const set = (next: string[]) =>
          onChange(
            next.length === 0
              ? deleteSuggestionMapping(row.providerId, row.key)
              : putSuggestionMapping(row.providerId, row.key, next),
          );
        const unmapped = row.projects.length === 0;
        return (
          <li
            key={`${row.providerId}:${row.key}`}
            className="flex flex-wrap items-center gap-2 border-t border-surface-700/40 px-3 py-2.5 first:border-t-0"
          >
            <span className="font-mono text-xs text-text-secondary">
              {row.key}
              {multi && (
                <span className="text-text-dim"> ({providers.find((p) => p.id === row.providerId)?.name})</span>
              )}
            </span>
            <span className="text-text-dim">→</span>
            {unmapped ? (
              <span className="text-xs text-text-dim">no mapping: the wizard asks</span>
            ) : (
              row.projects.map((path) => (
                <span key={path} className="inline-flex items-center gap-1 font-mono text-xs text-text-primary">
                  {name(path)}
                  <button
                    type="button"
                    aria-label={`Remove ${name(path)} from ${row.key}`}
                    disabled={disabled}
                    onClick={() => void set(row.projects.filter((p) => p !== path))}
                    className="text-text-dim hover:text-text-primary disabled:opacity-50"
                  >
                    <X className="h-3 w-3" />
                  </button>
                </span>
              ))
            )}
            <span className="flex-1" />
            <select
              aria-label={unmapped ? `Map ${row.key}` : `Add Project to ${row.key}`}
              value=""
              disabled={disabled || free.length === 0}
              onChange={(e) => e.target.value && void set([...row.projects, e.target.value])}
              className="h-8 rounded-md border border-surface-700 bg-surface-800 px-2 text-xs text-text-primary disabled:opacity-50"
            >
              <option value="">{unmapped ? "Map" : "+ Project"}</option>
              {free.map((p) => (
                <option key={p.path} value={p.path}>
                  {p.name}
                </option>
              ))}
            </select>
          </li>
        );
      })}
    </ul>
  );
}

interface Props {
  snapshot: SuggestionsSnapshot;
  disabled: boolean;
  onBack: () => void;
  onSource: (providerId: string, criterion: string, enabled: boolean) => Promise<void>;
}

export function SuggestionTemplates({ snapshot, disabled, onBack, onSource }: Props) {
  const [templates, setTemplates] = useState<SuggestionTemplate[]>([]);
  const [mappings, setMappings] = useState<SuggestionMapping[]>([]);
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const mounted = useRef(true);

  const load = useCallback(async () => {
    const [t, m, p] = await Promise.all([fetchSuggestionTemplates(), fetchSuggestionMappings(), fetchProjects({})]);
    if (!mounted.current) return;
    if (t) setTemplates(t);
    if (m) setMappings(m);
    if (p) setProjects(p);
    if (!t || !m) setError("Could not load templates and mappings.");
  }, []);

  useEffect(() => {
    mounted.current = true;
    // Deferred so the first load does not set state inside the effect body.
    const first = setTimeout(() => void load(), 0);
    return () => {
      mounted.current = false;
      clearTimeout(first);
    };
  }, [load]);

  const mutate = async (request: Promise<SuggestionResult<unknown>>) => {
    setError(null);
    const result = await request;
    if (mounted.current && !result.ok) setError(result.error);
    await load();
  };

  return (
    <div className="space-y-6">
      <button type="button" onClick={onBack} className="cursor-pointer text-sm text-brand-500">
        &larr; Suggestions
      </button>
      {error && (
        <div
          role="alert"
          className="rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
        >
          {error}
        </div>
      )}
      {snapshot.providers.map((provider) => (
        <section key={provider.id} aria-label={`${provider.name} prompt templates`}>
          <SectionHeader>{`${provider.name} · prompt templates`}</SectionHeader>
          <div className="space-y-3">
            {provider.sources.map((source) => (
              <TemplateBlock
                key={source.criterion}
                provider={provider}
                source={source}
                override={
                  templates.find((t) => t.provider_id === provider.id && t.criterion === source.criterion)?.template ??
                  null
                }
                item={snapshot.items.find((i) => i.provider_id === provider.id && i.criterion === source.criterion)}
                disabled={disabled}
                onSource={onSource}
                onSaved={load}
                onError={setError}
              />
            ))}
          </div>
        </section>
      ))}
      <section aria-label="Project mapping">
        <SectionHeader>Project mapping</SectionHeader>
        <MappingRows
          providers={snapshot.providers}
          items={snapshot.items}
          mappings={mappings}
          projects={projects}
          disabled={disabled}
          onChange={mutate}
        />
      </section>
    </div>
  );
}

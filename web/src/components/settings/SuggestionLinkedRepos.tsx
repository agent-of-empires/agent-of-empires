import { useCallback, useEffect, useRef, useState } from "react";
import { X } from "lucide-react";
import {
  deleteSuggestionMapping,
  fetchProjects,
  fetchSuggestionMappings,
  putSuggestionMapping,
  type SuggestionResult,
} from "../../lib/api";
import type {
  ProjectInfo,
  SuggestionItem,
  SuggestionMapping,
  SuggestionProvider,
  SuggestionsSnapshot,
} from "../../lib/types";
import { SuggestionMappingWizard } from "../SuggestionMappingWizard";

const GHOST =
  "inline-flex h-8 items-center rounded-md px-3 text-xs font-medium text-text-secondary hover:bg-surface-800 disabled:opacity-50";

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
              <span className="text-xs text-text-dim">no Project yet</span>
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
              aria-label={unmapped ? `Link ${row.key}` : `Add Project to ${row.key}`}
              value=""
              disabled={disabled || free.length === 0}
              onChange={(e) => e.target.value && void set([...row.projects, e.target.value])}
              className="h-8 rounded-md border border-surface-700 bg-surface-800 px-2 text-xs text-text-primary disabled:opacity-50"
            >
              <option value="">{unmapped ? "Link" : "+ Project"}</option>
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
}

/** Which Project each repo starts in; the same links "Start in…" makes. */
export function SuggestionLinkedRepos({ snapshot, disabled, onBack }: Props) {
  const [mappings, setMappings] = useState<SuggestionMapping[]>([]);
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [wizard, setWizard] = useState(false);
  const [saving, setSaving] = useState(false);
  const [mappingsStale, setMappingsStale] = useState(false);
  const mounted = useRef(true);

  const loadSeq = useRef(0);
  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    const [m, p] = await Promise.all([fetchSuggestionMappings(), fetchProjects({})]);
    if (!mounted.current || seq !== loadSeq.current) return;
    if (m) setMappings(m);
    setMappingsStale(!m);
    if (p) setProjects(p);
    const failed = [!m && "linked repos", !p && "Projects"].filter(Boolean);
    setLoadError(failed.length > 0 ? `Could not load ${failed.join(", ")}.` : null);
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

  // Controls stay off until the reload lands, and while it has failed: each edit is built from the rendered row.
  const mutate = async (request: Promise<SuggestionResult<unknown>>) => {
    setError(null);
    setSaving(true);
    const result = await request;
    if (mounted.current && !result.ok) setError(result.error);
    await load();
    if (mounted.current) setSaving(false);
  };

  return (
    <div className="space-y-4">
      <button type="button" onClick={onBack} className="cursor-pointer text-sm text-brand-500">
        &larr; Suggestions
      </button>
      <h2 className="text-base font-semibold text-text-primary">Linked repos</h2>
      {(error ?? loadError) && (
        <div
          role="alert"
          className="rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
        >
          {error ?? loadError}
        </div>
      )}
      <button
        type="button"
        className={GHOST}
        disabled={disabled || !snapshot.items.some((i) => i.mapping_key)}
        onClick={() => setWizard(true)}
      >
        Link repos automatically…
      </button>
      <MappingRows
        providers={snapshot.providers}
        items={snapshot.items}
        mappings={mappings}
        projects={projects}
        disabled={disabled || saving || mappingsStale}
        onChange={mutate}
      />
      {mappingsStale && (
        <p className="flex items-center gap-3 text-xs text-status-error">
          Could not reload linked repos.
          <button type="button" className={GHOST} onClick={() => void load()}>
            Retry
          </button>
        </p>
      )}
      {wizard && (
        <SuggestionMappingWizard
          items={snapshot.items}
          providers={snapshot.providers}
          disabled={disabled}
          onClose={() => setWizard(false)}
          onSaved={load}
        />
      )}
    </div>
  );
}

import { useCallback, useEffect, useState } from "react";
import { fetchSuggestionMappings, fetchSuggestionProjects, putSuggestionMapping } from "../lib/api";
import { proposeMappings, type MappingProposal } from "../lib/mappingProposals";
import { notifySuggestionsChanged } from "../lib/suggestionEvents";
import type { SuggestionItem, SuggestionProject, SuggestionProvider } from "../lib/types";
import { BRAND_BUTTON, CancelButton, Dialog } from "./Dialog";
import { useConfirmKeys } from "./dialogHooks";

interface Props {
  items: SuggestionItem[];
  providers: SuggestionProvider[];
  /** Saving is unavailable, as on a read-only dashboard or offline. */
  disabled?: boolean;
  onClose: () => void;
  /** At least one mapping was stored. */
  onSaved?: () => void;
}

interface Row extends MappingProposal {
  checked: boolean;
}

const rowId = (row: MappingProposal) => `${row.provider_id}\0${row.mapping_key}`;

/** Proposes a Project for every unmapped source key and stores the checked ones through the mapping API. */
export function SuggestionMappingWizard({ items, providers, disabled = false, onClose, onSaved }: Props) {
  const [projects, setProjects] = useState<SuggestionProject[]>([]);
  const [rows, setRows] = useState<Row[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    const [mappings, registered] = await Promise.all([fetchSuggestionMappings(), fetchSuggestionProjects()]);
    if (!mappings || !registered) {
      setError("Could not load Projects and mappings.");
      setRows([]);
      return;
    }
    setProjects(registered);
    setRows(proposeMappings(items, mappings, registered).map((p) => ({ ...p, checked: p.match === "origin" })));
  }, [items]);

  useEffect(() => {
    // Deferred so the first load does not set state inside the effect body.
    const first = setTimeout(() => void load(), 0);
    return () => clearTimeout(first);
    // Loads once; later snapshot reloads must not reset the user's choices.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const update = (id: string, patch: Partial<Row>) =>
    setRows((current) => current?.map((r) => (rowId(r) === id ? { ...r, ...patch } : r)) ?? null);

  const chosen = rows?.filter((r) => r.checked && r.project) ?? [];
  const save = async () => {
    if (busy || disabled || chosen.length === 0) return;
    setBusy(true);
    setError(null);
    const failures: string[] = [];
    for (const row of chosen) {
      const result = await putSuggestionMapping(row.provider_id, row.mapping_key, [row.project!]);
      if (!result.ok) failures.push(`${row.mapping_key}: ${result.error}`);
    }
    setBusy(false);
    if (failures.length < chosen.length) {
      notifySuggestionsChanged();
      onSaved?.();
    }
    if (failures.length === 0) return onClose();
    setError(`Could not save ${failures.join("; ")}`);
    await load();
  };
  useConfirmKeys(onClose, () => void save(), busy);

  const multi = providers.length > 1;
  const providerName = (id: string) => providers.find((p) => p.id === id)?.name ?? id;
  const proposed = rows?.filter((r) => r.match !== null) ?? [];
  const unmatched = rows?.filter((r) => r.match === null) ?? [];

  const renderRow = (row: Row) => {
    const id = rowId(row);
    return (
      <li key={id} className="flex flex-wrap items-center gap-2 py-2" data-testid="mapping-wizard-row">
        <input
          type="checkbox"
          aria-label={`Map ${row.mapping_key}`}
          checked={row.checked && row.project !== null}
          disabled={row.project === null || busy}
          onChange={(e) => update(id, { checked: e.target.checked })}
          className="h-4 w-4 accent-brand-500"
        />
        <span className="min-w-0 flex-1 break-all font-mono text-xs text-text-secondary">
          {row.mapping_key}
          {multi && <span className="text-text-dim"> ({providerName(row.provider_id)})</span>}
          <span className="text-text-dim"> · {row.count}</span>
        </span>
        {row.match && (
          <span className="rounded border border-surface-700 px-1.5 text-[11px] text-text-dim">
            {row.match === "origin" ? "git origin" : "same name"}
          </span>
        )}
        <select
          aria-label={`Project for ${row.mapping_key}`}
          value={row.project ?? ""}
          disabled={busy}
          onChange={(e) => update(id, { project: e.target.value || null, checked: e.target.value !== "" })}
          className="h-8 max-w-[220px] rounded-md border border-surface-700 bg-surface-800 px-2 text-xs text-text-primary disabled:opacity-50"
        >
          <option value="">Choose a Project</option>
          {projects.map((p) => (
            <option key={p.path} value={p.path}>
              {p.name}
            </option>
          ))}
        </select>
      </li>
    );
  };

  return (
    <Dialog
      id="suggestion-mapping-wizard"
      title="Map projects"
      panelClassName="w-[640px]"
      bodyClassName="max-h-[60vh] overflow-y-auto px-5 py-4"
      describedBy="suggestion-mapping-wizard-intro"
      onDismiss={onClose}
      footer={
        <>
          <CancelButton onClick={onClose} disabled={busy} />
          <button
            type="button"
            onClick={() => void save()}
            disabled={busy || disabled || chosen.length === 0}
            className={`rounded-md px-3 py-1.5 text-sm font-medium disabled:opacity-50 ${BRAND_BUTTON}`}
          >
            {busy ? "Saving…" : `Save ${chosen.length} ${chosen.length === 1 ? "mapping" : "mappings"}`}
          </button>
        </>
      }
    >
      <p id="suggestion-mapping-wizard-intro" className="text-sm text-text-secondary">
        Sources in the current suggestions without a Project. A Project whose git origin is the source is checked; one
        with the same name is proposed unchecked.
      </p>
      {error && (
        <div
          role="alert"
          className="mt-3 rounded-md border border-status-error/40 bg-status-error/10 p-3 text-sm text-status-error"
        >
          {error}
        </div>
      )}
      {rows === null ? (
        <p role="status" className="py-6 text-center text-sm text-text-dim">
          Loading…
        </p>
      ) : rows.length === 0 ? (
        !error && <p className="py-6 text-center text-sm text-text-dim">Every source in the suggestions is mapped.</p>
      ) : (
        <>
          {proposed.length > 0 && (
            <section aria-label="Proposed" className="mt-3">
              <h3 className="text-xs font-medium text-text-secondary">Proposed</h3>
              <ul className="divide-y divide-surface-700/40">{proposed.map(renderRow)}</ul>
            </section>
          )}
          {unmatched.length > 0 && (
            <section aria-label="No match" className="mt-3">
              <h3 className="text-xs font-medium text-text-secondary">No match: choose a Project</h3>
              <ul className="divide-y divide-surface-700/40">{unmatched.map(renderRow)}</ul>
            </section>
          )}
        </>
      )}
    </Dialog>
  );
}

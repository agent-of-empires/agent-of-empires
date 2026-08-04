import { useCallback, useEffect, useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { TextMessagePartProvider } from "@assistant-ui/react";
import {
  adoptSkill,
  createSkill,
  deleteSkill,
  fetchSkill,
  fetchSkills,
  syncSkills,
  updateSkill,
  type SkillDetail,
  type SkillRoot,
  type SkillSummary,
  type SkillSyncOutcome,
  type SkillsResponse,
} from "../lib/api";
import { ProvenanceBadge } from "./ProvenanceBadge";
import { labelForProvenance } from "../lib/skillProvenance";
import { Markdown } from "./acp/Markdown";
import { skillBody } from "../lib/skillBody";

function sourceId(skill: SkillSummary): string {
  return skill.provenance.kind === "aoe-managed" ? "aoe-managed" : skill.provenance.root;
}

function skillKey(skill: SkillSummary): string {
  return `${sourceId(skill)}:${skill.directory}`;
}

/** Compact counts line for a sync run, e.g. "3 shared, 1 unchanged, 1 conflict".
 *  "shared" folds together "created" and "updated" since both put a skill into
 *  an agent's directory; "unchanged" is omitted from the detail list below but
 *  still counted here so the user sees the full picture. */
function summarizeSyncOutcomes(outcomes: SkillSyncOutcome[]): string {
  const counts = { shared: 0, removed: 0, unchanged: 0, conflict: 0, error: 0 };
  for (const outcome of outcomes) {
    if (outcome.status === "created" || outcome.status === "updated") counts.shared += 1;
    else if (outcome.status === "removed") counts.removed += 1;
    else if (outcome.status === "unchanged") counts.unchanged += 1;
    else if (outcome.status === "conflict") counts.conflict += 1;
    else if (outcome.status === "error") counts.error += 1;
  }
  const parts: string[] = [];
  if (counts.shared) parts.push(`${counts.shared} shared`);
  if (counts.removed) parts.push(`${counts.removed} removed`);
  if (counts.unchanged) parts.push(`${counts.unchanged} unchanged`);
  if (counts.conflict) parts.push(`${counts.conflict} conflict${counts.conflict === 1 ? "" : "s"}`);
  if (counts.error) parts.push(`${counts.error} error${counts.error === 1 ? "" : "s"}`);
  return parts.length ? parts.join(", ") : "Nothing to sync.";
}

/** Fold a follow-up sync's outcomes into the displayed list: rows sharing a
 *  (root, directory) key are replaced in place so the rest of the panel
 *  (other roots, other skills) does not disappear, and any outcome the
 *  follow-up introduces that was not already shown is appended. */
function mergeSyncOutcomes(current: SkillSyncOutcome[] | null, updates: SkillSyncOutcome[]): SkillSyncOutcome[] {
  const key = (outcome: SkillSyncOutcome) => `${outcome.root}:${outcome.directory}`;
  const updateMap = new Map(updates.map((outcome) => [key(outcome), outcome]));
  const merged = (current ?? []).map((outcome) => updateMap.get(key(outcome)) ?? outcome);
  for (const outcome of updates) {
    if (!merged.some((existing) => key(existing) === key(outcome))) merged.push(outcome);
  }
  return merged;
}

/** One collapsible section of the sidebar list ("Managed" or "Available to
 *  adopt"). Both groups render the same row shape; only the membership and
 *  the section label differ, so the row markup lives here once. */
function SkillGroup({
  title,
  skills,
  roots,
  selectedKey,
  collapsed,
  onToggle,
  onSelect,
}: {
  title: string;
  skills: SkillSummary[];
  roots: SkillRoot[];
  selectedKey: string | null;
  collapsed: boolean;
  onToggle: () => void;
  onSelect: (skill: SkillSummary) => void;
}) {
  return (
    <div>
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={!collapsed}
        className="flex w-full items-center justify-between px-3 py-2 font-mono text-[11px] uppercase tracking-wider text-text-dim hover:text-text-secondary"
      >
        <span>{title}</span>
        {collapsed ? <ChevronRight className="h-3.5 w-3.5" /> : <ChevronDown className="h-3.5 w-3.5" />}
      </button>
      {!collapsed &&
        skills.map((skill) => (
          <button
            type="button"
            key={skillKey(skill)}
            onClick={() => onSelect(skill)}
            className={`block w-full border-b border-l-2 border-surface-800 px-3 py-2 text-left transition-colors ${
              skillKey(skill) === selectedKey
                ? "border-l-brand-500 bg-brand-600/10"
                : "border-l-transparent hover:bg-surface-800"
            }`}
          >
            <div className="flex items-center justify-between gap-2">
              <span className="truncate text-[13px] font-medium text-text-primary">{skill.directory}</span>
              <ProvenanceBadge
                label={labelForProvenance(skill.provenance, roots)}
                tone={skill.provenance.kind === "aoe-managed" ? "primary" : "neutral"}
              />
            </div>
            <p className="mt-1 line-clamp-2 text-[11px] leading-4 text-text-dim">{skill.description}</p>
          </button>
        ))}
    </div>
  );
}

/** The right pane's tab row. A single tab today (the raw/preview toggle lives
 *  separately, since it applies within this tab); kept as a list so adding a
 *  second tab (e.g. usage statistics) is a one-entry change. */
const DETAIL_TABS = [{ id: "content", label: "SKILL.md" }] as const;

export function SkillsManager({ readOnly = false }: { readOnly?: boolean } = {}) {
  const [data, setData] = useState<SkillsResponse | null>(null);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [detail, setDetail] = useState<SkillDetail | null>(null);
  const [draft, setDraft] = useState("");
  const [search, setSearch] = useState("");
  const [hideManagedExternal, setHideManagedExternal] = useState(true);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [loadError, setLoadError] = useState(false);
  const [newDirectory, setNewDirectory] = useState("");
  const [newDescription, setNewDescription] = useState("");
  const [syncOutcomes, setSyncOutcomes] = useState<SkillSyncOutcome[] | null>(null);
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [managedCollapsed, setManagedCollapsed] = useState(false);
  const [adoptCollapsed, setAdoptCollapsed] = useState(false);
  const [viewMode, setViewMode] = useState<"raw" | "preview">("raw");

  const load = useCallback(async (preferredKey?: string) => {
    const next = await fetchSkills();
    if (!next) {
      setLoadError(true);
      return;
    }
    setLoadError(false);
    setData(next);
    if (next.skills.length === 0) {
      setDetail(null);
      setDraft("");
    }
    setSelectedKey((current) => {
      const preferred = preferredKey ?? current;
      if (preferred && next.skills.some((skill) => skillKey(skill) === preferred)) {
        return preferred;
      }
      return next.skills[0] ? skillKey(next.skills[0]) : null;
    });
  }, []);

  useEffect(() => {
    const first = setTimeout(() => void load(), 0);
    return () => clearTimeout(first);
  }, [load]);

  const selected = data?.skills.find((skill) => skillKey(skill) === selectedKey) ?? null;
  const dirty = detail !== null && draft !== detail.content;

  // Keyed on the selection's primitives, NOT the `selected` object: that object
  // is a fresh `.find()` result on every render, so depending on it re-ran this
  // effect after any `load()` and reset the draft out from under an unsaved
  // edit. Sharing, replacing a conflict, and saving all keep the same skill
  // selected, so with primitive deps they no longer touch the editor at all.
  const selectedSource = selected ? sourceId(selected) : null;
  const selectedDirectory = selected?.directory ?? null;

  useEffect(() => {
    if (!selectedSource || !selectedDirectory) {
      return;
    }
    let cancelled = false;
    const read = async () => {
      const next = await fetchSkill(selectedSource, selectedDirectory);
      if (!cancelled) {
        setDetail(next);
        setDraft(next?.content ?? "");
        if (!next) setNotice("Could not read the selected skill.");
      }
    };
    void read();
    return () => {
      cancelled = true;
    };
  }, [selectedSource, selectedDirectory]);

  useEffect(() => {
    if (!dirty) return;
    const warn = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty]);

  /** Every path that moves the selection off an edited skill has to ask first,
   *  because moving it is what discards the draft. */
  const confirmDiscard = () => !dirty || window.confirm("Discard unsaved changes to this skill?");

  const select = (skill: SkillSummary) => {
    if (!confirmDiscard()) return;
    setNotice(null);
    setSelectedKey(skillKey(skill));
  };

  const create = async () => {
    // Creating jumps the selection to the new skill, so it discards a draft the
    // same way clicking another row does.
    if (!confirmDiscard()) return;
    setBusy(true);
    const result = await createSkill(newDirectory, newDescription || undefined);
    setBusy(false);
    if (!result.ok) {
      setNotice(result.error ?? "Could not create skill.");
      return;
    }
    const key = `aoe-managed:${newDirectory}`;
    setNewDirectory("");
    setNewDescription("");
    setShowCreateForm(false);
    setNotice("Managed skill created.");
    await load(key);
  };

  const sync = async () => {
    setBusy(true);
    const result = await syncSkills();
    setBusy(false);
    if (!result.ok) {
      setSyncOutcomes(null);
      setNotice(result.error ?? "Could not sync skills.");
      return;
    }
    setNotice(null);
    setSyncOutcomes(result.outcomes);
    await load(selectedKey ?? undefined);
  };

  /** Re-run sync for a single conflict, naming it in `replace` so the backend
   *  overwrites it instead of leaving it alone. Merges the follow-up's
   *  outcomes into the panel instead of replacing it wholesale, so the other
   *  rows already shown do not vanish. */
  const replaceConflict = async (outcome: SkillSyncOutcome) => {
    setBusy(true);
    const result = await syncSkills({ roots: [outcome.root], replace: [outcome.directory] });
    setBusy(false);
    if (!result.ok) {
      setNotice(result.error ?? "Could not replace skill.");
      return;
    }
    setNotice(null);
    setSyncOutcomes((current) => mergeSyncOutcomes(current, result.outcomes));
    await load(selectedKey ?? undefined);
  };

  /** Share only the selected skill: the server reconciles just that
   *  directory and skips orphan removal for the rest of the library, so the
   *  outcome panel reports exactly what happened to the one the user is
   *  looking at. */
  const shareSkill = async () => {
    if (!selected) return;
    setBusy(true);
    const result = await syncSkills({ directories: [selected.directory] });
    setBusy(false);
    if (!result.ok) {
      setSyncOutcomes(null);
      setNotice(result.error ?? "Could not share skill.");
      return;
    }
    setNotice(null);
    setSyncOutcomes(result.outcomes);
    await load(selectedKey ?? undefined);
  };

  const adopt = async () => {
    if (!selected || selected.provenance.kind !== "external") return;
    // Adopting selects the new managed copy, moving off whatever is being edited.
    if (!confirmDiscard()) return;
    setBusy(true);
    const result = await adoptSkill(selected.provenance.root, selected.directory);
    setBusy(false);
    if (!result.ok) {
      setNotice(result.error ?? "Could not adopt skill.");
      return;
    }
    setNotice("Skill adopted into AoE's managed store.");
    await load(`aoe-managed:${result.directory ?? selected.directory}`);
  };

  const save = async () => {
    if (!selected?.writable) return;
    setBusy(true);
    const result = await updateSkill(selected.directory, draft);
    setBusy(false);
    if (!result.ok) {
      setNotice(result.error ?? "Could not save skill.");
      return;
    }
    setDetail((current) => (current ? { ...current, content: draft } : current));
    setNotice("Skill saved.");
    await load(selectedKey ?? undefined);
  };

  const discard = () => {
    if (detail) setDraft(detail.content);
  };

  const remove = async () => {
    if (!selected?.writable || !window.confirm(`Delete managed skill "${selected.directory}"?`)) return;
    setBusy(true);
    const result = await deleteSkill(selected.directory);
    setBusy(false);
    if (!result.ok) {
      setNotice(result.error ?? "Could not delete skill.");
      return;
    }
    setDetail(null);
    setNotice("Managed skill deleted.");
    await load();
  };

  const normalized = search.trim().toLowerCase();
  const matchesSearch = (skill: SkillSummary) =>
    !normalized ||
    skill.directory.toLowerCase().includes(normalized) ||
    skill.name.toLowerCase().includes(normalized) ||
    skill.description.toLowerCase().includes(normalized);
  const managedDirectories = new Set(
    data?.skills.filter((skill) => skill.provenance.kind === "aoe-managed").map((skill) => skill.directory) ?? [],
  );
  const managedSkills =
    data?.skills.filter((skill) => skill.provenance.kind === "aoe-managed" && matchesSearch(skill)) ?? [];
  const adoptableSkills =
    data?.skills.filter(
      (skill) =>
        skill.provenance.kind === "external" &&
        matchesSearch(skill) &&
        (!hideManagedExternal || !managedDirectories.has(skill.directory)),
    ) ?? [];

  if (loadError) {
    return (
      <div className="rounded-lg border border-status-error/30 bg-status-error/10 p-4 text-[13px] text-status-error">
        Could not load skills.{" "}
        <button onClick={() => void load()} className="underline">
          Retry
        </button>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="rounded-lg border border-surface-700/60 bg-surface-850/70 p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <h3 className="font-mono text-sm uppercase tracking-widest text-text-primary">Skills Library</h3>
            <p className="mt-1 text-[12px] text-text-dim">
              Browse skills installed for other agents. Adopt a skill before editing it so the original stays untouched.
            </p>
          </div>
          <div className="flex items-center gap-2">
            <button
              type="button"
              disabled={readOnly}
              onClick={() => setShowCreateForm((current) => !current)}
              className="h-8 cursor-pointer rounded-md bg-brand-600 px-3 text-[12px] font-semibold text-text-on-brand transition-colors duration-150 hover:bg-brand-500 disabled:cursor-not-allowed disabled:opacity-40"
            >
              + New skill
            </button>
            <button
              type="button"
              disabled={busy || readOnly}
              onClick={() => void sync()}
              className="h-8 cursor-pointer rounded-md border border-surface-700 bg-surface-800 px-3 text-[12px] font-semibold text-text-secondary transition-colors duration-150 hover:bg-surface-700 disabled:cursor-not-allowed disabled:opacity-40"
            >
              Share with all agents
            </button>
          </div>
        </div>
        {readOnly && (
          <p className="mt-3 border-t border-surface-700/60 pt-3 text-[12px] text-text-dim">
            This server is read-only, so skills cannot be created, edited, or shared here.
          </p>
        )}
        {showCreateForm && (
          <div className="mt-3 grid gap-2 border-t border-surface-700/60 pt-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.5fr)_auto]">
            <input
              aria-label="New skill directory"
              value={newDirectory}
              onChange={(event) => setNewDirectory(event.target.value)}
              placeholder="new-skill"
              className="rounded border border-surface-700 bg-surface-900 px-3 py-2 font-mono text-[12px] text-text-primary outline-none focus:border-brand-500"
            />
            <input
              aria-label="New skill description"
              value={newDescription}
              onChange={(event) => setNewDescription(event.target.value)}
              placeholder="When should agents use it?"
              className="rounded border border-surface-700 bg-surface-900 px-3 py-2 text-[12px] text-text-primary outline-none focus:border-brand-500"
            />
            <button
              type="button"
              disabled={busy || readOnly || !newDirectory.trim()}
              onClick={() => void create()}
              className="h-8 cursor-pointer rounded-md bg-brand-600 px-4 text-[12px] font-semibold text-text-on-brand transition-colors duration-150 hover:bg-brand-500 disabled:cursor-not-allowed disabled:opacity-40"
            >
              Create
            </button>
          </div>
        )}
      </div>

      {notice && (
        <div
          role="status"
          className="rounded border border-surface-700 bg-surface-800 px-3 py-2 text-[12px] text-text-secondary"
        >
          {notice}
        </div>
      )}

      {syncOutcomes && (
        <div
          role="status"
          className="rounded border border-surface-700 bg-surface-800 px-3 py-2 text-[12px] text-text-secondary"
        >
          <p>{summarizeSyncOutcomes(syncOutcomes)}</p>
          {syncOutcomes.some((outcome) => outcome.status === "conflict" || outcome.status === "error") && (
            <ul className="mt-2 space-y-1 font-mono text-[11px] text-text-dim">
              {syncOutcomes
                .filter((outcome) => outcome.status === "conflict" || outcome.status === "error")
                .map((outcome, index) => (
                  <li key={`${outcome.root}:${outcome.directory}:${index}`} className="flex items-center gap-2">
                    <span>
                      {outcome.status} {outcome.root}/{outcome.directory}
                      {outcome.message ? `: ${outcome.message}` : ""}
                    </span>
                    {outcome.status === "conflict" && (
                      <button
                        type="button"
                        disabled={busy || readOnly}
                        onClick={() => void replaceConflict(outcome)}
                        aria-label={`Replace ${outcome.directory} in ${outcome.root}`}
                        className="h-8 cursor-pointer rounded-md border border-status-error/40 px-2 text-[11px] text-status-error transition-colors duration-150 hover:bg-status-error/10 disabled:cursor-not-allowed disabled:opacity-40"
                      >
                        Replace
                      </button>
                    )}
                  </li>
                ))}
            </ul>
          )}
        </div>
      )}

      {/* Explicit height: the settings content area (SettingsView) is itself
          a scroll container with no fixed height, so a plain h-full/flex-1
          pane here has nothing to measure against and collapses to its
          content height instead of scrolling internally. Pinning a height
          on the grid lets each pane scroll independently within it. */}
      <div className="grid h-[calc(100vh-16rem)] min-h-[26rem] gap-4 lg:grid-cols-[320px_minmax(0,1fr)]">
        <aside className="flex min-h-0 flex-col overflow-y-auto rounded-lg border border-surface-700/60 bg-surface-850/70">
          <div className="sticky top-0 z-10 space-y-2 border-b border-surface-700/60 bg-surface-850/95 p-3">
            <input
              aria-label="Search skills"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder="Search skills"
              className="w-full rounded border border-surface-700 bg-surface-900 px-3 py-2 text-[12px] text-text-primary outline-none focus:border-brand-500"
            />
            <label className="flex items-center gap-2 text-[11px] text-text-secondary">
              <input
                type="checkbox"
                checked={hideManagedExternal}
                onChange={(event) => setHideManagedExternal(event.target.checked)}
                className="h-3.5 w-3.5 rounded border-surface-600 bg-surface-900 text-brand-500"
              />
              Hide external skills already managed
            </label>
          </div>
          <SkillGroup
            title={`Managed (${managedSkills.length})`}
            skills={managedSkills}
            roots={data?.roots ?? []}
            selectedKey={selectedKey}
            collapsed={managedCollapsed}
            onToggle={() => setManagedCollapsed((current) => !current)}
            onSelect={select}
          />
          <SkillGroup
            title={`Available to adopt (${adoptableSkills.length})`}
            skills={adoptableSkills}
            roots={data?.roots ?? []}
            selectedKey={selectedKey}
            collapsed={adoptCollapsed}
            onToggle={() => setAdoptCollapsed((current) => !current)}
            onSelect={select}
          />
          {data && managedSkills.length === 0 && adoptableSkills.length === 0 && (
            <p className="p-4 text-[12px] text-text-dim">No matching skills.</p>
          )}
        </aside>

        <section className="flex min-h-0 flex-col overflow-y-auto rounded-lg border border-surface-700/60 bg-surface-850/70">
          {!selected && (
            <div className="flex h-full items-center justify-center p-6">
              <p className="text-[12px] text-text-dim">Select a skill to inspect its instructions.</p>
            </div>
          )}
          {selected && (
            <>
              <div className="sticky top-0 z-10 space-y-3 border-b border-surface-700/60 bg-surface-850/95 p-4">
                <div className="flex items-center gap-2">
                  <h4 className="text-lg font-semibold text-text-bright">{selected.directory}</h4>
                  <ProvenanceBadge
                    label={labelForProvenance(selected.provenance, data?.roots ?? [])}
                    tone={selected.provenance.kind === "aoe-managed" ? "primary" : "neutral"}
                  />
                </div>
                <p className="font-mono text-[11px]">
                  <span className="text-text-dim">{sourceId(selected)}</span>
                  <span className="text-text-muted"> / </span>
                  <span className="text-brand-500">{selected.directory}</span>
                </p>
                <div className="flex items-center justify-between gap-3">
                  <div className="flex gap-4">
                    {DETAIL_TABS.map((tab) => (
                      <span
                        key={tab.id}
                        className="border-b-2 border-brand-500 pb-1 font-mono text-[11px] uppercase tracking-wider text-brand-500"
                      >
                        {tab.label}
                      </span>
                    ))}
                  </div>
                  {/* Segmented toggle chips, not standalone action buttons: kept
                      below the 32px button height so the pair reads as one
                      compact control sitting at the tab-label baseline rather
                      than a second row of full-size buttons. */}
                  <div className="flex items-center gap-1 rounded-md bg-surface-900 p-0.5">
                    <button
                      type="button"
                      onClick={() => setViewMode("raw")}
                      className={`cursor-pointer rounded-md px-2 py-1 text-[11px] font-medium transition-colors duration-150 ${
                        viewMode === "raw"
                          ? "bg-brand-600 text-text-on-brand"
                          : "text-text-secondary hover:text-text-primary"
                      }`}
                    >
                      Raw
                    </button>
                    <button
                      type="button"
                      onClick={() => setViewMode("preview")}
                      className={`cursor-pointer rounded-md px-2 py-1 text-[11px] font-medium transition-colors duration-150 ${
                        viewMode === "preview"
                          ? "bg-brand-600 text-text-on-brand"
                          : "text-text-secondary hover:text-text-primary"
                      }`}
                    >
                      Preview
                    </button>
                  </div>
                </div>
              </div>

              {/* min-h-0 lets the editor shrink inside the flex column so it
                  grows to the bottom of the pane instead of sitting at a fixed
                  height with dead space above the footer. */}
              <div className="flex min-h-0 flex-1 flex-col p-4">
                {detail ? (
                  viewMode === "raw" ? (
                    <textarea
                      aria-label="SKILL.md content"
                      readOnly={!selected.writable || readOnly}
                      value={draft}
                      onChange={(event) => setDraft(event.target.value)}
                      spellCheck={false}
                      className="min-h-[16rem] w-full flex-1 resize-none rounded-md border border-surface-700 bg-surface-950 p-4 font-mono text-[12px] leading-5 text-text-primary outline-none focus:border-brand-500 read-only:text-text-secondary"
                    />
                  ) : (
                    <div className="min-h-[16rem] flex-1 overflow-y-auto rounded-md border border-surface-700 bg-surface-950 p-4">
                      <TextMessagePartProvider text={skillBody(draft)}>
                        <Markdown text={skillBody(draft)} />
                      </TextMessagePartProvider>
                    </div>
                  )
                ) : (
                  <p className="text-[13px] text-text-dim">Loading skill...</p>
                )}
              </div>

              <div className="sticky bottom-0 z-10 flex items-center justify-between gap-3 border-t border-surface-700/60 bg-surface-850/95 p-3">
                {selected.writable ? (
                  <button
                    type="button"
                    disabled={busy || readOnly}
                    onClick={() => void remove()}
                    className="h-8 cursor-pointer rounded-md border border-status-error/40 px-3 text-[12px] text-status-error transition-colors duration-150 hover:bg-status-error/10 disabled:cursor-not-allowed disabled:opacity-40"
                  >
                    Delete
                  </button>
                ) : (
                  <p className="text-[11px] text-text-dim">
                    External skills are read-only. Adopt this package to make an editable AoE-managed copy.
                  </p>
                )}
                <div className="flex items-center gap-3">
                  {selected.writable ? (
                    <>
                      <span className={`text-[11px] ${dirty ? "text-status-warning" : "text-status-running"}`}>
                        {dirty ? "Unsaved changes" : "All changes saved"}
                      </span>
                      <button
                        type="button"
                        disabled={busy || dirty || readOnly}
                        title={dirty ? "Save or discard your changes before sharing" : undefined}
                        onClick={() => void shareSkill()}
                        className="h-8 cursor-pointer rounded-md border border-surface-700 bg-surface-800 px-3 text-[12px] text-text-secondary transition-colors duration-150 disabled:cursor-not-allowed disabled:opacity-40"
                      >
                        Share this skill
                      </button>
                      <button
                        type="button"
                        disabled={busy || !dirty || readOnly}
                        onClick={discard}
                        className="h-8 cursor-pointer rounded-md border border-surface-700 bg-surface-800 px-3 text-[12px] text-text-secondary transition-colors duration-150 disabled:cursor-not-allowed disabled:opacity-40"
                      >
                        Discard
                      </button>
                      <button
                        type="button"
                        disabled={busy || !dirty || readOnly}
                        onClick={() => void save()}
                        className="h-8 cursor-pointer rounded-md bg-brand-600 px-4 text-[12px] font-semibold text-text-on-brand transition-colors duration-150 hover:bg-brand-500 disabled:cursor-not-allowed disabled:opacity-40"
                      >
                        Save
                      </button>
                    </>
                  ) : (
                    <button
                      type="button"
                      disabled={busy || readOnly}
                      onClick={() => void adopt()}
                      className="h-8 cursor-pointer rounded-md bg-brand-600 px-3 text-[12px] font-semibold text-text-on-brand transition-colors duration-150 hover:bg-brand-500 disabled:cursor-not-allowed disabled:opacity-40"
                    >
                      Adopt into AoE
                    </button>
                  )}
                </div>
              </div>
            </>
          )}
        </section>
      </div>
    </div>
  );
}

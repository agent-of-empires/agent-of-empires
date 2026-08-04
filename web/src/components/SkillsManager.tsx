import { useCallback, useEffect, useState } from "react";
import {
  adoptSkill,
  createSkill,
  deleteSkill,
  fetchSkill,
  fetchSkills,
  updateSkill,
  type SkillDetail,
  type SkillSummary,
  type SkillsResponse,
} from "../lib/api";

function sourceId(skill: SkillSummary): string {
  return skill.provenance.kind === "aoe-managed" ? "aoe-managed" : skill.provenance.root;
}

function skillKey(skill: SkillSummary): string {
  return `${sourceId(skill)}:${skill.directory}`;
}

function SourceBadge({ skill }: { skill: SkillSummary }) {
  const external = skill.provenance.kind === "external";
  return (
    <span
      className={`rounded px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-wider ${
        external ? "bg-accent-600/15 text-accent-500" : "bg-brand-600/15 text-brand-400"
      }`}
    >
      {skill.provenance.kind === "external" ? skill.provenance.root : "managed"}
    </span>
  );
}

export function SkillsManager() {
  const [data, setData] = useState<SkillsResponse | null>(null);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [detail, setDetail] = useState<SkillDetail | null>(null);
  const [draft, setDraft] = useState("");
  const [search, setSearch] = useState("");
  const [root, setRoot] = useState("all");
  const [hideManagedExternal, setHideManagedExternal] = useState(true);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [loadError, setLoadError] = useState(false);
  const [newDirectory, setNewDirectory] = useState("");
  const [newDescription, setNewDescription] = useState("");

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

  useEffect(() => {
    if (!selected) {
      return;
    }
    let cancelled = false;
    const read = async () => {
      const next = await fetchSkill(sourceId(selected), selected.directory);
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
  }, [selected]);

  useEffect(() => {
    if (!dirty) return;
    const warn = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty]);

  const select = (skill: SkillSummary) => {
    if (dirty && !window.confirm("Discard unsaved changes to this skill?")) return;
    setNotice(null);
    setSelectedKey(skillKey(skill));
  };

  const create = async () => {
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
    setNotice("Managed skill created.");
    await load(key);
  };

  const adopt = async () => {
    if (!selected || selected.provenance.kind !== "external") return;
    setBusy(true);
    const result = await adoptSkill(selected.provenance.root, selected.directory);
    setBusy(false);
    if (!result.ok) {
      setNotice(result.error ?? "Could not adopt skill.");
      return;
    }
    setNotice("Skill adopted into AoE's managed store.");
    setRoot("all");
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
  const managedDirectories = new Set(
    data?.skills.filter((skill) => skill.provenance.kind === "aoe-managed").map((skill) => skill.directory) ?? [],
  );
  const visible =
    data?.skills.filter(
      (skill) =>
        (root === "all" || sourceId(skill) === root) &&
        (!hideManagedExternal || skill.provenance.kind === "aoe-managed" || !managedDirectories.has(skill.directory)) &&
        (!normalized ||
          skill.directory.toLowerCase().includes(normalized) ||
          skill.name.toLowerCase().includes(normalized) ||
          skill.description.toLowerCase().includes(normalized)),
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
        <div className="mb-3">
          <h3 className="font-mono text-sm uppercase tracking-widest text-text-primary">Skills Library</h3>
          <p className="mt-1 text-[12px] text-text-dim">
            Browse skills installed for other agents. Adopt a skill before editing it so the original stays untouched.
          </p>
        </div>
        <div className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.5fr)_auto]">
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
            disabled={busy || !newDirectory.trim()}
            onClick={() => void create()}
            className="rounded bg-brand-600 px-4 py-2 text-[12px] font-semibold text-white hover:bg-brand-500 disabled:opacity-40"
          >
            Create
          </button>
        </div>
      </div>

      {notice && (
        <div
          role="status"
          className="rounded border border-surface-700 bg-surface-800 px-3 py-2 text-[12px] text-text-secondary"
        >
          {notice}
        </div>
      )}

      <div className="grid min-h-[34rem] overflow-hidden rounded-lg border border-surface-700/60 bg-surface-900 lg:grid-cols-[20rem_minmax(0,1fr)]">
        <aside className="border-b border-surface-700/60 bg-surface-850/45 lg:border-b-0 lg:border-r">
          <div className="grid gap-2 border-b border-surface-700/60 p-3">
            <input
              aria-label="Search skills"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder="Search skills"
              className="rounded border border-surface-700 bg-surface-900 px-3 py-2 text-[12px] text-text-primary outline-none focus:border-brand-500"
            />
            <select
              aria-label="Filter skills by source"
              value={root}
              onChange={(event) => setRoot(event.target.value)}
              className="rounded border border-surface-700 bg-surface-900 px-3 py-2 font-mono text-[11px] text-text-secondary"
            >
              <option value="all">All sources</option>
              <option value="aoe-managed">AoE managed</option>
              {data?.roots.map((entry) => (
                <option key={entry.id} value={entry.id}>
                  {entry.label}
                  {entry.legacy ? " (legacy)" : ""}
                </option>
              ))}
            </select>
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
          <div className="max-h-[28rem] overflow-y-auto lg:max-h-[40rem]">
            {visible.map((skill) => (
              <button
                type="button"
                key={skillKey(skill)}
                onClick={() => select(skill)}
                className={`w-full border-b border-surface-700/40 px-3 py-3 text-left transition-colors ${
                  skillKey(skill) === selectedKey
                    ? "bg-brand-600/10 shadow-[inset_3px_0_0_var(--color-brand-500)]"
                    : "hover:bg-surface-800"
                }`}
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="truncate font-mono text-[12px] font-medium text-text-primary">
                    {skill.directory}
                  </span>
                  <SourceBadge skill={skill} />
                </div>
                <p className="mt-1 line-clamp-2 text-[11px] leading-4 text-text-dim">{skill.description}</p>
              </button>
            ))}
            {data && visible.length === 0 && <p className="p-4 text-[12px] text-text-dim">No matching skills.</p>}
          </div>
        </aside>

        <section className="min-w-0 p-4 sm:p-5">
          {!selected && <p className="text-[13px] text-text-dim">Select a skill to inspect its instructions.</p>}
          {selected && (
            <div className="space-y-4">
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div>
                  <div className="flex items-center gap-2">
                    <h4 className="font-mono text-lg font-semibold text-text-primary">{selected.name}</h4>
                    <SourceBadge skill={selected} />
                  </div>
                  <p className="mt-1 text-[12px] text-text-secondary">{selected.description}</p>
                  <p className="mt-2 font-mono text-[10px] text-text-muted">
                    {sourceId(selected)} / {selected.directory}
                  </p>
                </div>
                {selected.provenance.kind === "external" && (
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() => void adopt()}
                    className="rounded bg-brand-600 px-3 py-1.5 text-[12px] font-semibold text-white hover:bg-brand-500 disabled:opacity-40"
                  >
                    Adopt into AoE
                  </button>
                )}
              </div>

              {detail ? (
                <>
                  <textarea
                    aria-label="SKILL.md content"
                    readOnly={!selected.writable}
                    value={draft}
                    onChange={(event) => setDraft(event.target.value)}
                    spellCheck={false}
                    className="min-h-[25rem] w-full resize-y rounded-md border border-surface-700 bg-surface-950 p-4 font-mono text-[12px] leading-5 text-text-primary outline-none focus:border-brand-500 read-only:text-text-secondary"
                  />
                  {selected.writable ? (
                    <div className="flex items-center justify-between gap-3">
                      <button
                        type="button"
                        disabled={busy}
                        onClick={() => void remove()}
                        className="rounded border border-status-error/40 px-3 py-1.5 text-[12px] text-status-error hover:bg-status-error/10 disabled:opacity-40"
                      >
                        Delete
                      </button>
                      <div className="flex items-center gap-3">
                        {dirty && <span className="text-[11px] text-status-waiting">Unsaved changes</span>}
                        <button
                          type="button"
                          disabled={busy || !dirty}
                          onClick={() => void save()}
                          className="rounded bg-brand-600 px-4 py-1.5 text-[12px] font-semibold text-white hover:bg-brand-500 disabled:opacity-40"
                        >
                          Save
                        </button>
                      </div>
                    </div>
                  ) : (
                    <p className="text-[11px] text-text-dim">
                      External skills are read-only. Adopt this package to make an editable AoE-managed copy.
                    </p>
                  )}
                </>
              ) : (
                <p className="text-[13px] text-text-dim">Loading skill...</p>
              )}
            </div>
          )}
        </section>
      </div>
    </div>
  );
}

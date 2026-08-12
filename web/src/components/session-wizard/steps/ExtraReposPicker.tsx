import { useEffect, useState } from "react";
import type { ProjectInfo } from "../../../lib/types";
import { fetchBranches, fetchProjects, type BranchInfo } from "../../../lib/api";

interface Props {
  primaryPath: string;
  selectedPaths: string[];
  onChange: (paths: string[]) => void;
  /** Base branch per repo path. Empty or missing means the repo falls back to
   *  the session-wide base branch. See #3329. */
  repoBases: Record<string, string>;
  onRepoBasesChange: (bases: Record<string, string>) => void;
  /** False while attaching to an existing branch, when no branch is created
   *  and a base has nothing to apply to. */
  basesEnabled: boolean;
}

/// Base-branch typeahead for one extra repo. The branch list comes from that
/// repo's own path, so a workspace member offers its own epic branches rather
/// than the launch repo's. Free text is accepted: a ref that exists only on a
/// remote still resolves at worktree creation. See #3329.
function RepoBaseInput({
  repoPath,
  label,
  value,
  onChange,
}: {
  repoPath: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
}) {
  const [branches, setBranches] = useState<BranchInfo[] | null>(null);
  const [focused, setFocused] = useState(false);

  useEffect(() => {
    if (!focused || branches !== null) return;
    let cancelled = false;
    fetchBranches(repoPath, true).then((rows) => {
      if (!cancelled) setBranches(rows ?? []);
    });
    return () => {
      cancelled = true;
    };
  }, [focused, branches, repoPath]);

  const query = value.trim().toLowerCase();
  const suggestions = (branches ?? []).filter((b) => !query || b.name.toLowerCase().includes(query)).slice(0, 6);

  return (
    <div className="relative flex-1 min-w-0">
      <input
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onFocus={() => setFocused(true)}
        onBlur={() => setTimeout(() => setFocused(false), 120)}
        placeholder="base branch (optional)"
        aria-label={`Base branch for ${label}`}
        autoComplete="off"
        className="w-full px-2 py-1 text-[12px] bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 font-mono"
      />
      {focused && suggestions.length > 0 && (
        <ul
          role="listbox"
          aria-label={`Branch suggestions for ${label}`}
          className="absolute left-0 right-0 top-full z-20 mt-1 max-h-48 overflow-y-auto bg-surface-900 border border-surface-700/60 rounded-md shadow-lg"
        >
          {suggestions.map((b) => (
            <li
              key={`${b.name}-${b.remote_only ? "r" : "l"}`}
              role="option"
              aria-selected={b.name === value}
              onMouseDown={(e) => {
                e.preventDefault();
                onChange(b.name);
                setFocused(false);
              }}
              className="px-2 py-1 text-[12px] font-mono cursor-pointer text-text-secondary hover:bg-surface-800 hover:text-text-primary"
            >
              {b.name}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export function ExtraReposPicker({
  primaryPath,
  selectedPaths,
  onChange,
  repoBases,
  onRepoBasesChange,
  basesEnabled,
}: Props) {
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [freeText, setFreeText] = useState("");

  useEffect(() => {
    fetchProjects().then((p) => {
      setProjects(p);
      setLoading(false);
    });
  }, []);

  const setRepoBase = (path: string, base: string) => {
    const next = { ...repoBases };
    if (base.trim()) next[path] = base;
    else delete next[path];
    onRepoBasesChange(next);
  };

  // Hide the primary repo from the picker so users can't accidentally
  // duplicate it (the builder rejects duplicate repo names).
  const pickable = projects.filter((p) => p.path !== primaryPath);

  const isSelected = (path: string) => selectedPaths.includes(path);

  const toggle = (path: string) => {
    if (isSelected(path)) {
      onChange(selectedPaths.filter((p) => p !== path));
    } else {
      onChange([...selectedPaths, path]);
    }
  };

  const addFreeText = () => {
    const trimmed = freeText.trim();
    if (!trimmed) return;
    if (selectedPaths.includes(trimmed) || trimmed === primaryPath) {
      setFreeText("");
      return;
    }
    onChange([...selectedPaths, trimmed]);
    setFreeText("");
  };

  const removePath = (path: string) => {
    onChange(selectedPaths.filter((p) => p !== path));
    setRepoBase(path, "");
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-2">
        <h3 className="text-sm font-medium text-text-primary">Extra repos (optional)</h3>
        <span className="text-[11px] text-text-dim">
          {selectedPaths.length > 0 ? `${selectedPaths.length} selected` : "none"}
        </span>
      </div>
      <p className="text-[11px] text-text-dim mb-3">
        Include additional repositories in the same workspace. Each gets its own worktree on the same branch, forked
        from the session's base branch unless you give it one of its own.
      </p>

      {selectedPaths.length > 0 && (
        <div className="flex flex-col gap-1.5 mb-3">
          {selectedPaths.map((path) => {
            const known = projects.find((p) => p.path === path);
            const label = known?.name || path.split("/").filter(Boolean).pop() || path;
            return (
              <div key={path} className="flex items-center gap-1.5">
                <span
                  className="inline-flex items-center gap-1.5 px-2 py-1 bg-brand-600/20 border border-brand-600/40 rounded-md text-[12px] text-text-primary shrink-0"
                  title={path}
                >
                  <span className="font-mono">{label}</span>
                  <button
                    type="button"
                    onClick={() => removePath(path)}
                    className="text-text-dim hover:text-text-primary cursor-pointer"
                    aria-label={`Remove ${label}`}
                  >
                    &times;
                  </button>
                </span>
                {basesEnabled && (
                  <RepoBaseInput
                    repoPath={path}
                    label={label}
                    value={repoBases[path] ?? ""}
                    onChange={(v) => setRepoBase(path, v)}
                  />
                )}
              </div>
            );
          })}
        </div>
      )}

      {!loading && pickable.length > 0 && (
        <div className="mb-3">
          <p className="text-[10px] uppercase tracking-wider text-text-dim mb-1.5">Registered projects</p>
          <div className="flex flex-wrap gap-1.5">
            {pickable.map((p) => (
              <button
                key={p.path}
                type="button"
                onClick={() => toggle(p.path)}
                className={`inline-flex items-center gap-1.5 px-2 py-1 rounded-md text-[12px] cursor-pointer transition-colors ${
                  isSelected(p.path)
                    ? "bg-brand-600/20 border border-brand-600/40 text-text-primary"
                    : "bg-surface-900 border border-surface-700/40 text-text-secondary hover:border-surface-700"
                }`}
                title={p.path}
              >
                <span className="font-mono">{p.name}</span>
                <span className="text-[9px] uppercase text-text-dim">{p.scope}</span>
              </button>
            ))}
          </div>
        </div>
      )}

      {!loading && pickable.length === 0 && projects.length === 0 && (
        <p className="text-[11px] text-text-dim mb-3">
          No registered projects yet. Add one with{" "}
          <code className="text-text-secondary">aoe project add &lt;path&gt;</code> or via the Projects page.
        </p>
      )}

      <div className="flex gap-2">
        <input
          type="text"
          value={freeText}
          onChange={(e) => setFreeText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              addFreeText();
            }
          }}
          placeholder="/path/to/another/repo"
          className="flex-1 px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 font-mono"
        />
        <button
          type="button"
          onClick={addFreeText}
          disabled={!freeText.trim()}
          className={`px-3 py-2 text-sm rounded-md transition-colors ${
            !freeText.trim()
              ? "bg-surface-800 text-text-dim cursor-not-allowed"
              : "bg-surface-700 hover:bg-surface-600 text-text-primary cursor-pointer"
          }`}
        >
          Add
        </button>
      </div>
    </div>
  );
}

import { useEffect, useState } from "react";
import type { ProjectInfo } from "../../../lib/types";
import { fetchProjects } from "../../../lib/api";

interface Props {
  primaryPath: string;
  selectedPaths: string[];
  onChange: (paths: string[]) => void;
}

export function ExtraReposPicker({
  primaryPath,
  selectedPaths,
  onChange,
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
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-2">
        <h3 className="text-sm font-medium text-text-primary">
          Extra repos (optional)
        </h3>
        <span className="text-[11px] text-text-dim">
          {selectedPaths.length > 0
            ? `${selectedPaths.length} selected`
            : "none"}
        </span>
      </div>
      <p className="text-[11px] text-text-dim mb-3">
        Include additional repositories in the same workspace. Each gets its own
        worktree on the same branch.
      </p>

      {selectedPaths.length > 0 && (
        <div className="flex flex-wrap gap-1.5 mb-3">
          {selectedPaths.map((path) => {
            const known = projects.find((p) => p.path === path);
            const label =
              known?.name || path.split("/").filter(Boolean).pop() || path;
            return (
              <span
                key={path}
                className="inline-flex items-center gap-1.5 px-2 py-1 bg-brand-600/20 border border-brand-600/40 rounded-md text-[12px] text-text-primary"
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
            );
          })}
        </div>
      )}

      {!loading && pickable.length > 0 && (
        <div className="mb-3">
          <p className="text-[10px] uppercase tracking-wider text-text-dim mb-1.5">
            Registered projects
          </p>
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
                <span className="text-[9px] uppercase text-text-dim">
                  {p.scope}
                </span>
              </button>
            ))}
          </div>
        </div>
      )}

      {!loading && pickable.length === 0 && projects.length === 0 && (
        <p className="text-[11px] text-text-dim mb-3">
          No registered projects yet. Add one with{" "}
          <code className="text-text-secondary">
            aoe project add &lt;path&gt;
          </code>{" "}
          or via the Projects page.
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

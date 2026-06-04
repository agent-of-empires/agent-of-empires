import { useEffect, useState } from "react";
import type { ProjectInfo } from "../lib/types";
import { fetchProjects, createProject, deleteProject } from "../lib/api";
import { DirectoryBrowser } from "./DirectoryBrowser";

interface Props {
  onClose: () => void;
  readOnly?: boolean;
}

export function ProjectsView({ onClose, readOnly }: Props) {
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [showAdd, setShowAdd] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Add-form state
  const [path, setPath] = useState("");
  const [name, setName] = useState("");
  const [baseBranch, setBaseBranch] = useState("");
  const [scope, setScope] = useState<"global" | "profile">("global");
  const [allowOverride, setAllowOverride] = useState(false);
  const [showBrowser, setShowBrowser] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  const reload = async () => {
    setLoading(true);
    const list = await fetchProjects();
    setProjects(list);
    setLoading(false);
  };

  useEffect(() => {
    reload();
  }, []);

  const handleAdd = async () => {
    const trimmedPath = path.trim();
    if (!trimmedPath) return;
    setSubmitting(true);
    setError(null);
    const result = await createProject({
      path: trimmedPath,
      name: name.trim() || undefined,
      scope,
      allow_override: allowOverride || undefined,
      default_base_branch: baseBranch.trim() || undefined,
    });
    setSubmitting(false);
    if (!result.ok) {
      setError(result.error || "Add failed");
      return;
    }
    setPath("");
    setName("");
    setBaseBranch("");
    setScope("global");
    setAllowOverride(false);
    setShowAdd(false);
    setShowBrowser(false);
    await reload();
  };

  const handleRemove = async (project: ProjectInfo) => {
    if (!confirm(`Remove project '${project.name}' from ${project.scope} scope?`)) return;
    setError(null);
    const result = await deleteProject(project.name, project.scope);
    if (!result.ok) {
      setError(result.error || "Remove failed");
      return;
    }
    await reload();
  };

  return (
    <div className="flex flex-col h-full bg-surface-900">
      <div className="flex items-center justify-between px-4 py-3 border-b border-surface-700/30">
        <div>
          <h1 className="text-lg font-semibold text-text-primary">Projects</h1>
          <p className="text-xs text-text-dim">
            Saved repositories you can multi-select when creating sessions.
          </p>
        </div>
        <div className="flex gap-2">
          {!readOnly && !showAdd && (
            <button
              type="button"
              onClick={() => setShowAdd(true)}
              className="px-3 py-1.5 text-sm bg-brand-600 hover:bg-brand-700 text-surface-900 rounded-md cursor-pointer font-medium"
            >
              + Add project
            </button>
          )}
          <button
            type="button"
            onClick={onClose}
            className="px-3 py-1.5 text-sm border border-surface-700 text-text-secondary hover:bg-surface-800 rounded-md cursor-pointer"
          >
            Close
          </button>
        </div>
      </div>

      {error && (
        <div className="mx-4 mt-3 px-3 py-2 bg-red-900/20 border border-red-700/30 rounded-md">
          <p className="text-sm text-red-400">{error}</p>
        </div>
      )}

      {showAdd && !readOnly && (
        <div className="px-4 pt-4">
          <div className="bg-surface-800 border border-surface-700/40 rounded-lg p-4">
            <h2 className="text-sm font-medium text-text-primary mb-3">Add project</h2>

            <label className="block text-[12px] text-text-dim mb-1">Path</label>
            <div className="flex gap-2 mb-3">
              <input
                type="text"
                value={path}
                onChange={(e) => setPath(e.target.value)}
                placeholder="/path/to/repo"
                className="flex-1 px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 font-mono"
              />
              <button
                type="button"
                onClick={() => setShowBrowser((b) => !b)}
                className="px-3 py-2 text-sm border border-surface-700 text-text-secondary hover:bg-surface-700/40 rounded-md cursor-pointer"
              >
                {showBrowser ? "Hide browser" : "Browse"}
              </button>
            </div>
            {showBrowser && (
              <div className="mb-3">
                <DirectoryBrowser
                  onSelect={(p) => {
                    setPath(p);
                    setShowBrowser(false);
                  }}
                />
              </div>
            )}

            <label className="block text-[12px] text-text-dim mb-1">Name (optional)</label>
            <input
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="defaults to directory name"
              className="w-full px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 mb-3"
            />

            <label className="block text-[12px] text-text-dim mb-1">Default base branch for extra repos (optional)</label>
            <input
              type="text"
              value={baseBranch}
              onChange={(e) => setBaseBranch(e.target.value)}
              placeholder="blank = inherit global default, then auto-detect"
              className="w-full px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 font-mono mb-3"
            />

            <label className="block text-[12px] text-text-dim mb-1">Scope</label>
            <div className="flex gap-2 mb-4">
              {(["global", "profile"] as const).map((s) => (
                <button
                  key={s}
                  type="button"
                  onClick={() => setScope(s)}
                  className={`px-3 py-1.5 text-sm rounded-md cursor-pointer transition-colors ${
                    scope === s
                      ? "bg-brand-600/20 border border-brand-600/40 text-text-primary"
                      : "bg-surface-900 border border-surface-700/40 text-text-secondary hover:border-surface-700"
                  }`}
                >
                  {s === "global" ? "Global (all profiles)" : "Profile-only"}
                </button>
              ))}
            </div>

            <label className="flex items-start gap-2 mb-4 cursor-pointer select-none">
              <input
                type="checkbox"
                checked={allowOverride}
                onChange={(e) => setAllowOverride(e.target.checked)}
                className="mt-0.5 cursor-pointer"
              />
              <span className="text-[12px] text-text-secondary">
                Allow override
                <span className="block text-text-dim text-[11px] mt-0.5">
                  Permit registering even if this path already exists in the
                  other scope. The profile entry will shadow the global one in
                  merged views.
                </span>
              </span>
            </label>

            <div className="flex justify-end gap-2">
              <button
                type="button"
                onClick={() => {
                  setShowAdd(false);
                  setShowBrowser(false);
                  setPath("");
                  setName("");
                  setBaseBranch("");
                  setScope("global");
                  setAllowOverride(false);
                  setError(null);
                }}
                className="px-3 py-1.5 text-sm border border-surface-700 text-text-secondary hover:bg-surface-800 rounded-md cursor-pointer"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={handleAdd}
                disabled={!path.trim() || submitting}
                className={`px-3 py-1.5 text-sm rounded-md font-medium ${
                  !path.trim() || submitting
                    ? "bg-brand-600/40 text-surface-900/60 cursor-not-allowed"
                    : "bg-brand-600 hover:bg-brand-700 text-surface-900 cursor-pointer"
                }`}
              >
                {submitting ? "Adding…" : "Add"}
              </button>
            </div>
          </div>
        </div>
      )}

      <div className="flex-1 overflow-y-auto p-4">
        {loading && (
          <div className="space-y-2 animate-pulse">
            {[...Array(3)].map((_, i) => (
              <div key={i} className="h-[60px] bg-surface-800/40 border border-surface-700/40 rounded-md" />
            ))}
          </div>
        )}

        {!loading && projects.length === 0 && (
          <div className="flex flex-col items-center justify-center py-16 text-center">
            <p className="text-sm text-text-secondary mb-1">No registered projects yet.</p>
            <p className="text-xs text-text-dim">
              Add one above, or use{" "}
              <code className="text-text-secondary">aoe project add &lt;path&gt;</code> from the CLI.
            </p>
          </div>
        )}

        {!loading && projects.length > 0 && (
          <ul className="space-y-2">
            {projects.map((p) => (
              <li
                key={`${p.scope}:${p.path}`}
                className="flex items-center gap-3 px-3 py-2.5 bg-surface-800/40 border border-surface-700/40 rounded-md"
              >
                <div className="flex-1 min-w-0">
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-medium text-text-primary">{p.name}</span>
                    <span
                      className={`text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded ${
                        p.scope === "global"
                          ? "bg-brand-600/20 text-text-primary"
                          : "bg-surface-700/60 text-text-secondary"
                      }`}
                    >
                      {p.scope}
                    </span>
                  </div>
                  <p className="text-[11px] font-mono text-text-dim truncate mt-0.5" title={p.path}>
                    {p.path}
                  </p>
                  {p.default_base_branch && (
                    <p className="text-[11px] text-text-dim mt-0.5">
                      base branch:{" "}
                      <span className="font-mono text-text-secondary">{p.default_base_branch}</span>
                    </p>
                  )}
                </div>
                {!readOnly && (
                  <button
                    type="button"
                    onClick={() => handleRemove(p)}
                    className="px-2 py-1 text-xs border border-surface-700 text-text-dim hover:text-status-error hover:border-status-error/40 rounded-md cursor-pointer"
                  >
                    Remove
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

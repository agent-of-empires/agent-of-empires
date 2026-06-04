import { useEffect, useState } from "react";
import type { ProjectInfo } from "../lib/types";
import { fetchProjects, createProject, deleteProject, updateProject } from "../lib/api";
import { DirectoryBrowser } from "./DirectoryBrowser";

interface Props {
  onClose: () => void;
  readOnly?: boolean;
}

export function ProjectsView({ onClose, readOnly }: Props) {
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [showAdd, setShowAdd] = useState(false);
  // The project currently being edited, or null when adding / closed. The form
  // modal opens when `showAdd` is true (add) or `editing` is set (edit).
  const [editing, setEditing] = useState<ProjectInfo | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Form state, shared by the add and edit modes.
  const [path, setPath] = useState("");
  const [name, setName] = useState("");
  const [baseBranch, setBaseBranch] = useState("");
  const [scope, setScope] = useState<"global" | "profile">("global");
  const [allowOverride, setAllowOverride] = useState(false);
  const [showBrowser, setShowBrowser] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  const isEdit = editing !== null;
  const formOpen = (showAdd || isEdit) && !readOnly;

  const reload = async () => {
    setLoading(true);
    const list = await fetchProjects();
    setProjects(list);
    setLoading(false);
  };

  useEffect(() => {
    const t = setTimeout(() => void reload(), 0);
    return () => clearTimeout(t);
  }, []);

  const resetForm = () => {
    setPath("");
    setName("");
    setBaseBranch("");
    setScope("global");
    setAllowOverride(false);
  };

  const openAdd = () => {
    if (submitting) return;
    setError(null);
    resetForm();
    setEditing(null);
    setShowAdd(true);
  };

  const openEdit = (p: ProjectInfo) => {
    if (submitting) return;
    setError(null);
    setShowAdd(false);
    setPath(p.path);
    setName(p.name);
    setBaseBranch(p.default_base_branch ?? "");
    setScope(p.scope);
    setAllowOverride(false);
    setEditing(p);
  };

  const closeForm = () => {
    if (submitting) return;
    setShowAdd(false);
    setEditing(null);
    setShowBrowser(false);
    resetForm();
    setError(null);
  };

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
    closeForm();
    await reload();
  };

  const handleSaveEdit = async () => {
    if (!editing) return;
    setSubmitting(true);
    setError(null);
    const result = await updateProject(
      editing.name,
      editing.scope,
      baseBranch.trim() || null,
    );
    setSubmitting(false);
    if (!result.ok) {
      setError(result.error || "Update failed");
      return;
    }
    closeForm();
    await reload();
  };

  const handleRemove = async (project: ProjectInfo) => {
    if (submitting) return;
    if (!confirm(`Remove project '${project.name}' from ${project.scope} scope?`)) return;
    setError(null);
    const result = await deleteProject(project.name, project.scope);
    if (!result.ok) {
      setError(result.error || "Remove failed");
      return;
    }
    await reload();
  };

  const lockedFieldClass =
    "w-full px-3 py-2 text-sm bg-surface-900/60 border border-surface-700/30 rounded-md text-text-dim cursor-not-allowed mb-3";

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
          {!readOnly && !formOpen && (
            <button
              type="button"
              onClick={openAdd}
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

      {formOpen && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
          onClick={submitting ? undefined : closeForm}
        >
          <div
            className="w-full max-w-lg max-h-[90vh] overflow-y-auto bg-surface-800 border border-surface-700/40 rounded-lg p-4"
            onClick={(e) => e.stopPropagation()}
          >
            <h2 className="text-sm font-medium text-text-primary mb-3">
              {isEdit ? `Edit project '${editing?.name}'` : "Add project"}
            </h2>

            <label className="block text-[12px] text-text-dim mb-1">Path</label>
            {isEdit ? (
              <input
                type="text"
                value={path}
                disabled
                title="Path is fixed; remove and re-add the project to change it"
                className={`${lockedFieldClass} font-mono`}
              />
            ) : (
              <>
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
              </>
            )}

            <label className="block text-[12px] text-text-dim mb-1">Name{isEdit ? "" : " (optional)"}</label>
            <input
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              disabled={isEdit}
              placeholder={isEdit ? undefined : "defaults to directory name"}
              title={isEdit ? "Rename is not supported yet; remove and re-add to rename" : undefined}
              className={
                isEdit
                  ? lockedFieldClass
                  : "w-full px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 mb-3"
              }
            />

            <label className="block text-[12px] text-text-dim mb-1">Default base branch (optional)</label>
            <input
              type="text"
              value={baseBranch}
              onChange={(e) => setBaseBranch(e.target.value)}
              placeholder="blank = inherit global default, then auto-detect"
              className="w-full px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 font-mono mb-1"
            />
            <p className="text-[11px] text-text-dim mb-3">
              Base branch new worktree branches for this project fork from. An
              explicit per-session base wins; blank inherits the global default,
              then the repo's detected default branch.
            </p>

            <label className="block text-[12px] text-text-dim mb-1">Scope</label>
            {isEdit ? (
              <p className="mb-4 text-sm text-text-secondary capitalize">{scope}</p>
            ) : (
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
            )}

            {!isEdit && (
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
            )}

            <div className="flex justify-end gap-2">
              <button
                type="button"
                onClick={closeForm}
                disabled={submitting}
                className="px-3 py-1.5 text-sm border border-surface-700 text-text-secondary hover:bg-surface-800 rounded-md cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={isEdit ? handleSaveEdit : handleAdd}
                disabled={(!isEdit && !path.trim()) || submitting}
                className={`px-3 py-1.5 text-sm rounded-md font-medium ${
                  (!isEdit && !path.trim()) || submitting
                    ? "bg-brand-600/40 text-surface-900/60 cursor-not-allowed"
                    : "bg-brand-600 hover:bg-brand-700 text-surface-900 cursor-pointer"
                }`}
              >
                {isEdit
                  ? submitting
                    ? "Saving…"
                    : "Save"
                  : submitting
                    ? "Adding…"
                    : "Add"}
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
                  <div className="flex gap-2">
                    <button
                      type="button"
                      onClick={() => openEdit(p)}
                      disabled={submitting}
                      className="px-2 py-1 text-xs border border-surface-700 text-text-dim hover:text-text-primary hover:border-surface-700 rounded-md cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                    >
                      Edit
                    </button>
                    <button
                      type="button"
                      onClick={() => handleRemove(p)}
                      disabled={submitting}
                      className="px-2 py-1 text-xs border border-surface-700 text-text-dim hover:text-status-error hover:border-status-error/40 rounded-md cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                    >
                      Remove
                    </button>
                  </div>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

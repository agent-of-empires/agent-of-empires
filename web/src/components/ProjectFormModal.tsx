import { useState } from "react";
import type { ProjectInfo } from "../lib/types";
import { createProject, updateProject } from "../lib/api";
import { DirectoryBrowser } from "./DirectoryBrowser";

interface Props {
  /** The project to edit, or null/undefined to add a new one. In edit mode
   *  only the default base branch is mutable; path, name, and scope are
   *  fixed (remove and re-add to change them). */
  initial?: ProjectInfo | null;
  onClose: () => void;
  /** Called after a successful create/update so the caller can refresh the
   *  registry. Awaited before the modal closes, so the section reflects the
   *  change by the time the form disappears (matching the pin/unpin handlers).
   *  May be sync or return a promise. */
  onSaved: () => void | Promise<void>;
}

const lockedFieldClass =
  "w-full px-3 py-2 text-sm bg-surface-900/60 border border-surface-700/30 rounded-md text-text-dim cursor-not-allowed mb-3";

// Add / edit form for a registered project, shared by the sidebar Projects
// section. Lifted out of the former full-page ProjectsView so the same form
// renders as a modal next to the sidebar. See #2212.
export function ProjectFormModal({ initial, onClose, onSaved }: Props) {
  const isEdit = initial != null;
  const [path, setPath] = useState(initial?.path ?? "");
  const [name, setName] = useState(initial?.name ?? "");
  const [baseBranch, setBaseBranch] = useState(initial?.default_base_branch ?? "");
  const [scope, setScope] = useState<"global" | "profile">(initial?.scope ?? "global");
  const [allowOverride, setAllowOverride] = useState(false);
  const [showBrowser, setShowBrowser] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const close = () => {
    if (submitting) return;
    onClose();
  };

  const handleSubmit = async () => {
    if (isEdit) {
      setSubmitting(true);
      setError(null);
      const result = await updateProject(initial.name, initial.scope, baseBranch.trim() || null);
      if (!result.ok) {
        setSubmitting(false);
        setError(result.error || "Update failed");
        return;
      }
      await onSaved();
      onClose();
      return;
    }

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
    if (!result.ok) {
      setSubmitting(false);
      setError(result.error || "Add failed");
      return;
    }
    await onSaved();
    onClose();
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      onClick={close}
      data-testid="project-form-modal"
    >
      <div
        className="w-full max-w-lg max-h-[90vh] overflow-y-auto bg-surface-800 border border-surface-700/40 rounded-lg p-4"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="text-sm font-medium text-text-primary mb-3">
          {isEdit ? `Edit project '${initial.name}'` : "Add project"}
        </h2>

        {error && (
          <div className="mb-3 px-3 py-2 bg-red-900/20 border border-red-700/30 rounded-md">
            <p className="text-sm text-red-400">{error}</p>
          </div>
        )}

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
          Base branch new worktree branches for this project fork from. An explicit per-session base wins; blank
          inherits the global default, then the repo's detected default branch.
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
                Permit registering even if this path already exists in the other scope. The profile entry will shadow
                the global one in merged views.
              </span>
            </span>
          </label>
        )}

        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={close}
            disabled={submitting}
            className="px-3 py-1.5 text-sm border border-surface-700 text-text-secondary hover:bg-surface-800 rounded-md cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={handleSubmit}
            disabled={(!isEdit && !path.trim()) || submitting}
            className={`px-3 py-1.5 text-sm rounded-md font-medium ${
              (!isEdit && !path.trim()) || submitting
                ? "bg-brand-600/40 text-surface-900/60 cursor-not-allowed"
                : "bg-brand-600 hover:bg-brand-700 text-surface-900 cursor-pointer"
            }`}
          >
            {isEdit ? (submitting ? "Saving…" : "Save") : submitting ? "Adding…" : "Add"}
          </button>
        </div>
      </div>
    </div>
  );
}

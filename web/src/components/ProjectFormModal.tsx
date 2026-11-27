import { useState } from "react";
import type { ProjectInfo } from "../lib/types";
import { createProject, projectTarget, updateProject } from "../lib/api";
import { DirectoryBrowser } from "./DirectoryBrowser";
import { type PluginSettingsDraft, ProjectPluginSettings } from "./ProjectPluginSettings";
import { hasFinePointer } from "../lib/platform";
import { BUTTON_SECONDARY, INPUT } from "../lib/surfaces";
import { CancelButton, ConfirmButton, Dialog } from "./Dialog";
import { FIELD_HINT, FIELD_LABEL, CHOICE, CHOICE_SELECTED } from "../lib/surfaces";

interface Props {
  profile: string;
  /** Editing preserves the registration identity and scope. */
  initial?: ProjectInfo | null;
  onClose: () => void;
  /** Refresh the registry before closing after a commit. */
  onSaved: () => void | Promise<void>;
}

const fieldClass = `${INPUT} mb-3`;
const lockedFieldClass = `${fieldClass} !text-text-dim cursor-not-allowed`;
const labelClass = `${FIELD_LABEL} !mb-1`;

export function ProjectFormModal({ initial, profile, onClose, onSaved }: Props) {
  const isEdit = initial != null;
  const [path, setPath] = useState(initial?.path ?? "");
  const [name, setName] = useState(initial?.name ?? "");
  const [baseBranch, setBaseBranch] = useState(initial?.default_base_branch ?? "");
  const [scope, setScope] = useState<"global" | "profile">(initial?.scope ?? "global");
  const [allowOverride, setAllowOverride] = useState(false);
  const [showBrowser, setShowBrowser] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  type OverrideChoice = "inherit" | "on" | "off";
  const overrideChoiceFrom = (v: boolean | undefined): OverrideChoice =>
    v === true ? "on" : v === false ? "off" : "inherit";
  const initialWorktreeChoice = overrideChoiceFrom(initial?.overrides?.worktree_enabled);
  const initialSmartRenameChoice = overrideChoiceFrom(initial?.overrides?.smart_rename);
  const initialSandboxChoice = overrideChoiceFrom(initial?.overrides?.sandbox_enabled);
  const [worktreeOverride, setWorktreeOverride] = useState<OverrideChoice>(initialWorktreeChoice);
  const [smartRenameOverride, setSmartRenameOverride] = useState<OverrideChoice>(initialSmartRenameChoice);
  const [sandboxOverride, setSandboxOverride] = useState<OverrideChoice>(initialSandboxChoice);
  const [pluginDraft, setPluginDraft] = useState<PluginSettingsDraft>({});

  const close = () => {
    if (submitting) return;
    onClose();
  };

  const handleSubmit = async () => {
    if (isEdit) {
      setSubmitting(true);
      setError(null);
      const overridesChanged =
        worktreeOverride !== initialWorktreeChoice ||
        smartRenameOverride !== initialSmartRenameChoice ||
        sandboxOverride !== initialSandboxChoice ||
        Object.keys(pluginDraft).length > 0;
      const toPatchValue = (c: OverrideChoice): boolean | null => (c === "inherit" ? null : c === "on");
      const result = await updateProject(initial.path, projectTarget(initial.scope, profile), {
        default_base_branch: baseBranch.trim() || null,
        ...(overridesChanged
          ? {
              overrides: {
                worktree_enabled: toPatchValue(worktreeOverride),
                smart_rename: toPatchValue(smartRenameOverride),
                sandbox_enabled: toPatchValue(sandboxOverride),
                ...(Object.keys(pluginDraft).length > 0 ? { plugin_settings: pluginDraft } : {}),
              },
            }
          : {}),
      });
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
      profile,
      allow_override: allowOverride || undefined,
      default_base_branch: baseBranch.trim() || undefined,
      overrides: {
        worktree_enabled: worktreeOverride === "inherit" ? undefined : worktreeOverride === "on",
        smart_rename: smartRenameOverride === "inherit" ? undefined : smartRenameOverride === "on",
        sandbox_enabled: sandboxOverride === "inherit" ? undefined : sandboxOverride === "on",
        plugin_settings: Object.keys(pluginDraft).length > 0 ? pluginDraft : undefined,
      },
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
    <Dialog
      id="project-form-modal"
      title={isEdit ? `Edit project '${initial.name}'` : "Add project"}
      describedBy={false}
      panelClassName="md:w-[32rem] md:max-h-[90vh]"
      onDismiss={close}
      footer={
        <>
          <CancelButton onClick={close} disabled={submitting} />
          <ConfirmButton onClick={handleSubmit} disabled={(!isEdit && !path.trim()) || submitting}>
            {isEdit ? (submitting ? "Saving…" : "Save") : submitting ? "Adding…" : "Add"}
          </ConfirmButton>
        </>
      }
    >
      {error && (
        <div className="mb-3 rounded-lg bg-status-error/10 p-3">
          <p className="text-[15px] md:text-sm text-status-error">{error}</p>
        </div>
      )}

      <label className={labelClass}>Path</label>
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
              autoFocus={hasFinePointer()}
              value={path}
              onChange={(e) => setPath(e.target.value)}
              placeholder="/path/to/repo"
              className="flex-1 px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 font-mono"
            />
            <button type="button" onClick={() => setShowBrowser((b) => !b)} className={`${BUTTON_SECONDARY} shrink-0`}>
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

      <label className={labelClass}>Name{isEdit ? "" : " (optional)"}</label>
      <input
        type="text"
        value={name}
        onChange={(e) => setName(e.target.value)}
        disabled={isEdit}
        placeholder={isEdit ? undefined : "defaults to directory name"}
        title={isEdit ? "Rename is not supported yet; remove and re-add to rename" : undefined}
        className={isEdit ? lockedFieldClass : fieldClass}
      />

      <label className={labelClass}>Default base branch (optional)</label>
      <input
        type="text"
        autoFocus={isEdit && hasFinePointer()}
        value={baseBranch}
        onChange={(e) => setBaseBranch(e.target.value)}
        placeholder="blank = inherit global default, then auto-detect"
        className={`${INPUT} font-mono mb-1`}
      />
      <p className={`${FIELD_HINT} mb-3`}>
        Base branch new worktree branches for this project fork from. An explicit per-session base wins; blank inherits
        the global default, then the repo's detected default branch.
      </p>

      <label className={labelClass}>Worktree by default</label>
      <select
        value={worktreeOverride}
        onChange={(e) => setWorktreeOverride(e.target.value as OverrideChoice)}
        className={fieldClass}
      >
        <option value="inherit">Use global default</option>
        <option value="on">On</option>
        <option value="off">Off</option>
      </select>

      <label className={labelClass}>Smart session rename</label>
      <select
        value={smartRenameOverride}
        onChange={(e) => setSmartRenameOverride(e.target.value as OverrideChoice)}
        className={fieldClass}
      >
        <option value="inherit">Use global default</option>
        <option value="on">On</option>
        <option value="off">Off</option>
      </select>

      <label className={labelClass}>Start in container by default</label>
      <select
        value={sandboxOverride}
        onChange={(e) => setSandboxOverride(e.target.value as OverrideChoice)}
        className={`${INPUT} mb-4`}
      >
        <option value="inherit">Use global default</option>
        <option value="on">On</option>
        <option value="off">Off</option>
      </select>

      <ProjectPluginSettings
        stored={initial?.overrides?.plugin_settings}
        draft={pluginDraft}
        onChange={setPluginDraft}
      />

      <label className={labelClass}>Scope</label>
      {isEdit ? (
        <p className="mb-4 text-sm text-text-secondary capitalize">{scope}</p>
      ) : (
        <div className="flex gap-2 mb-4">
          {(["global", "profile"] as const).map((s) => (
            <button
              key={s}
              type="button"
              onClick={() => setScope(s)}
              className={`${scope === s ? CHOICE_SELECTED : CHOICE} !w-auto max-md:flex-1 text-[15px] md:text-sm text-text-primary cursor-pointer`}
            >
              {s === "global" ? "Global (all profiles)" : "Profile-only"}
            </button>
          ))}
        </div>
      )}

      {!isEdit && (
        <label className="flex items-start gap-2 mb-1 cursor-pointer select-none">
          <input
            type="checkbox"
            checked={allowOverride}
            onChange={(e) => setAllowOverride(e.target.checked)}
            className="mt-0.5 cursor-pointer"
          />
          <span className="text-[15px] md:text-sm text-text-primary">
            Allow override
            <span className={`block mt-0.5 ${FIELD_HINT}`}>
              Permit registering even if this path already exists in the other scope. The profile entry will shadow the
              global one in merged views.
            </span>
          </span>
        </label>
      )}
    </Dialog>
  );
}

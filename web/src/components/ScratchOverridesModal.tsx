import { useEffect, useRef, useState } from "react";
import { fetchScratchOverrides, updateScratchOverrides } from "../lib/api";
import { BRAND_BUTTON, CancelButton, ConfirmButton, Dialog } from "./Dialog";
import { useConfirmKeys, useDialogFocus } from "./dialogHooks";

interface Props {
  onClose: () => void;
}

type OverrideChoice = "inherit" | "on" | "off";
const choiceFrom = (v: boolean | undefined): OverrideChoice => (v === true ? "on" : v === false ? "off" : "inherit");
const toPatchValue = (c: OverrideChoice): boolean | null => (c === "inherit" ? null : c === "on");

// Settings for the sidebar's synthetic Scratch group. Scratch sessions have no repo path to
// register a project entry under, so they get this dedicated, trimmed-down settings dialog
// instead of ProjectFormModal (no path/name/scope-registration/worktree fields apply). Built on
// the shared Dialog shell for dialog semantics, focus handling, and Escape-to-close.
export function ScratchOverridesModal({ onClose }: Props) {
  const [scope, setScope] = useState<"global" | "profile">("global");
  const [choice, setChoice] = useState<OverrideChoice>("inherit");
  // Derived rather than its own piece of state: as soon as `scope` changes this flips back to
  // true on its own, so Save can't fire against the previous scope's value while the refetch for
  // the new one is still in flight.
  const [loadedScope, setLoadedScope] = useState<"global" | "profile" | null>(null);
  const loading = loadedScope !== scope;
  const [loadError, setLoadError] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const saveButtonRef = useRef<HTMLButtonElement | null>(null);
  useDialogFocus(saveButtonRef);

  useEffect(() => {
    let cancelled = false;
    void fetchScratchOverrides(scope).then((overrides) => {
      if (cancelled) return;
      if (overrides === null) {
        setLoadError(true);
        setError("Failed to load current setting");
        setLoadedScope(scope);
        return;
      }
      setLoadError(false);
      setError(null);
      setChoice(choiceFrom(overrides.smart_rename));
      setLoadedScope(scope);
    });
    return () => {
      cancelled = true;
    };
  }, [scope]);

  const close = () => {
    if (submitting) return;
    onClose();
  };

  const handleSubmit = async () => {
    // The Save button's own `disabled` covers a click, but useConfirmKeys' Enter shortcut calls
    // this directly (SELECT isn't in its OWNS_ENTER exclusion list), bypassing that guard.
    if (loading || loadError) return;
    setSubmitting(true);
    setError(null);
    const result = await updateScratchOverrides(scope, toPatchValue(choice));
    if (!result.ok) {
      setSubmitting(false);
      setError(result.error || "Update failed");
      return;
    }
    onClose();
  };

  useConfirmKeys(close, handleSubmit, submitting);

  return (
    <Dialog
      id="scratch-overrides-modal"
      title="Scratch session settings"
      onDismiss={close}
      footer={
        <>
          <CancelButton onClick={close} disabled={submitting} />
          <ConfirmButton
            buttonRef={saveButtonRef}
            onClick={handleSubmit}
            busy={submitting || loading || loadError}
            className={BRAND_BUTTON}
            testId="scratch-overrides-save"
          >
            {submitting ? "Saving…" : "Save"}
          </ConfirmButton>
        </>
      }
    >
      {error && (
        <div className="mb-3 px-3 py-2 bg-red-900/20 border border-red-700/30 rounded-md">
          <p className="text-sm text-red-400">{error}</p>
        </div>
      )}

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

      <label htmlFor="scratch-smart-rename-select" className="block text-[12px] text-text-dim mb-1">
        Smart session rename
      </label>
      <select
        id="scratch-smart-rename-select"
        value={choice}
        disabled={loading}
        onChange={(e) => setChoice(e.target.value as OverrideChoice)}
        className="w-full px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary focus:outline-none focus:border-brand-600 mb-1"
      >
        <option value="inherit">Use global default</option>
        <option value="on">On</option>
        <option value="off">Off</option>
      </select>
      <p className="text-[11px] text-text-dim">
        Worktrees are never offered for scratch sessions (not a git repo), so there is no worktree-default setting here.
      </p>
    </Dialog>
  );
}

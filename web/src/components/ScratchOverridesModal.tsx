import { useEffect, useRef, useState } from "react";
import { fetchSettings, getProfileSettings, updateProfileSettings, updateSettings } from "../lib/api";
import { BRAND_BUTTON, CancelButton, ConfirmButton, Dialog } from "./Dialog";
import { useConfirmKeys, useDialogFocus } from "./dialogHooks";

interface Props {
  /** The server's own active profile (`ServerAbout.profile`): "profile" scope means this one. */
  profile: string;
  onClose: () => void;
}

type StoredChoice = "inherit" | "on" | "off";
const isStoredChoice = (v: unknown): v is StoredChoice => v === "inherit" || v === "on" || v === "off";

// "default" is a client-only pseudo-value: it means the profile has no `scratch_smart_rename`
// override at all (the key is absent from `GET /api/profiles/{name}/settings`'s sparse response),
// distinct from an explicit "inherit" override. Saving "default" PATCHes `null`, clearing the
// profile-level key entirely, so an untouched Save can't silently start overriding a global
// setting that isn't "inherit" (e.g. global On/Off) by writing an explicit "inherit" in its place.
// Meaningless at global scope: the global config is never sparse, so its fetch always returns a
// concrete value.
type Choice = StoredChoice | "default";

// Settings for the sidebar's synthetic Scratch group. Scratch sessions have no repo path to
// register a project entry under, so `session.scratch_smart_rename` (a normal schema-backed
// setting) is read and written through the existing global/profile settings API instead of a
// dedicated project-registry entry. Built on the shared Dialog shell for dialog semantics, focus
// handling, and Escape-to-close.
export function ScratchOverridesModal({ profile, onClose }: Props) {
  const [scope, setScope] = useState<"global" | "profile">("global");
  const [choice, setChoice] = useState<Choice>("inherit");
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
    const load = scope === "global" ? fetchSettings() : getProfileSettings(profile);
    void load.then((settings) => {
      if (cancelled) return;
      if (settings === null) {
        setLoadError(true);
        setError("Failed to load current setting");
        setLoadedScope(scope);
        return;
      }
      const session = settings.session as { scratch_smart_rename?: unknown } | undefined;
      setLoadError(false);
      setError(null);
      const raw = session?.scratch_smart_rename;
      setChoice(isStoredChoice(raw) ? raw : scope === "profile" ? "default" : "inherit");
      setLoadedScope(scope);
    });
    return () => {
      cancelled = true;
    };
  }, [scope, profile]);

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
    const patch = { session: { scratch_smart_rename: choice === "default" ? null : choice } };
    const ok = scope === "global" ? await updateSettings(patch) : await updateProfileSettings(profile, patch);
    if (!ok) {
      setSubmitting(false);
      setError("Update failed");
      return;
    }
    onClose();
  };

  useConfirmKeys(close, handleSubmit, submitting);

  return (
    <Dialog
      id="scratch-overrides-modal"
      title="Scratch session settings"
      describedBy={false}
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
        onChange={(e) => setChoice(e.target.value as Choice)}
        className="w-full px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary focus:outline-none focus:border-brand-600 mb-1"
      >
        {scope === "profile" && <option value="default">Use global default</option>}
        <option value="inherit">Inherit Smart Session Rename</option>
        <option value="on">On</option>
        <option value="off">Off</option>
      </select>
      <p className="text-[11px] text-text-dim">
        Worktrees are never offered for scratch sessions (not a git repo), so there is no worktree-default setting here.
      </p>
    </Dialog>
  );
}

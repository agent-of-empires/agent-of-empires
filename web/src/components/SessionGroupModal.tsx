import { useRef, useState } from "react";
import { CancelButton, ConfirmButton, Dialog } from "./Dialog";
import { useDialogFocus } from "./dialogHooks";

interface Props {
  sessionTitle: string;
  currentGroup: string;
  /** Resolves false on failure so the modal stays open with an error. */
  onSave: (group: string) => Promise<boolean>;
  onClose: () => void;
}

export function SessionGroupModal({ sessionTitle, currentGroup, onSave, onClose }: Props) {
  const [value, setValue] = useState(currentGroup);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);
  useDialogFocus(inputRef, true);

  const handleSave = async () => {
    // A blank value ungroups; paths are sent as-is, with no slash normalization.
    const next = value.trim();
    if (next === currentGroup) return onClose();
    setSaving(true);
    setError(null);
    if (await onSave(next)) return onClose();
    setSaving(false);
    setError(next ? "Failed to update group." : "Failed to clear group.");
    inputRef.current?.focus();
  };

  return (
    <Dialog
      id="session-group-modal"
      title="Edit group"
      bodyClassName="px-5 py-4 space-y-3"
      onDismiss={() => !saving && onClose()}
      footer={
        <>
          <CancelButton onClick={onClose} disabled={saving} />
          <ConfirmButton
            onClick={handleSave}
            busy={saving}
            testId="session-group-modal-save"
            className="text-white bg-brand-600/90 hover:bg-brand-600"
          >
            {saving ? "Saving..." : "Save"}
          </ConfirmButton>
        </>
      }
    >
      <p className="text-[13px] text-text-secondary">
        Move <span className="text-text-primary">{sessionTitle}</span> to a group.
      </p>
      <input
        ref={inputRef}
        type="text"
        value={value}
        onChange={(e) => {
          setValue(e.target.value);
          setError(null);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            if (!saving) void handleSave();
          }
          if (e.key === "Escape" && !saving) onClose();
        }}
        placeholder="Group (blank to ungroup)"
        data-testid="session-group-modal-input"
        className="w-full bg-surface-900 border border-surface-700 rounded px-2 py-1.5 text-[13px] font-mono text-text-primary focus:outline-none focus:border-brand-600"
      />
      <p className="text-[12px] text-text-dim">
        Leave blank to ungroup. Use <span className="font-mono">/</span> for hierarchy, for example{" "}
        <span className="font-mono">work/projects</span>.
      </p>
      {error && (
        <p data-testid="session-group-modal-error" className="text-[12px] text-status-error">
          {error}
        </p>
      )}
    </Dialog>
  );
}

import { useEffect, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { fetchGroups } from "../lib/api";
import { reportError } from "../lib/logger";
import type { GroupInfo } from "../lib/types";
import { CancelButton, ConfirmButton, Dialog } from "./Dialog";
import { isImeComposing } from "../lib/ime";
import { GroupCombobox } from "./session-wizard/steps/GroupCombobox";

const DIALOG_ID = "session-group-modal";
const PROMPT_ID = `${DIALOG_ID}-prompt`;

interface Props {
  sessionTitle: string;
  currentGroup: string;
  /** Profile whose groups are offered; the served profile when omitted. */
  profile?: string;
  /** Resolves false on failure so the modal stays open with an error. */
  onSave: (group: string) => Promise<boolean>;
  onClose: () => void;
}

export function SessionGroupModal({ sessionTitle, currentGroup, profile, onSave, onClose }: Props) {
  const [value, setValue] = useState(currentGroup);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refocusing, setRefocusing] = useState(false);
  // Tagged with their profile, so a profile change withholds the old list until the new one loads.
  const [loaded, setLoaded] = useState<{ profile?: string; groups: GroupInfo[] }>({ groups: [] });
  const groups = loaded.profile === profile ? loaded.groups : [];
  const fieldRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let live = true;
    fetchGroups(profile)
      .then((g) => live && setLoaded({ profile, groups: g }))
      .catch((err) => reportError(err, { level: "warn", target: "web.client" }));
    return () => {
      live = false;
    };
  }, [profile]);

  // A blank value ungroups; paths are sent as-is, with no slash normalization.
  const save = async (next: string) => {
    if (next === currentGroup) return onClose();
    setSaving(true);
    setError(null);
    if (await onSave(next)) return onClose();
    // Commit first so this refocus alone doesn't open the list over the error (`openOnFocus`).
    flushSync(() => {
      setSaving(false);
      setError(next ? "Failed to update group." : "Failed to clear group.");
      setRefocusing(true);
    });
    fieldRef.current?.querySelector("input")?.focus();
    setRefocusing(false);
  };
  const handleSave = () => save(value.trim());

  return (
    <Dialog
      id={DIALOG_ID}
      title="Edit group"
      describedBy={PROMPT_ID}
      bodyClassName="px-5 py-4 space-y-3 max-h-[calc(100vh-13rem)] overflow-y-auto"
      onDismiss={() => !saving && onClose()}
      footer={
        // Keeps input focus on press, so blur can't collapse the inline list and shift the buttons before the click.
        <div className="contents" onMouseDown={(e) => e.preventDefault()}>
          {currentGroup && (
            <button
              type="button"
              onClick={() => void save("")}
              disabled={saving}
              data-testid="session-group-modal-remove"
              className="mr-auto text-[13px] text-text-secondary hover:text-text-primary disabled:opacity-50"
            >
              Remove from group
            </button>
          )}
          <CancelButton onClick={onClose} disabled={saving} />
          <ConfirmButton
            onClick={handleSave}
            busy={saving}
            testId="session-group-modal-save"
            className="text-white bg-brand-600/90 hover:bg-brand-600"
          >
            {saving ? "Saving..." : "Save"}
          </ConfirmButton>
        </div>
      }
    >
      <p id={PROMPT_ID} className="text-[13px] text-text-secondary">
        Move <span className="text-text-primary">{sessionTitle}</span> to a group.
      </p>
      <div
        ref={fieldRef}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.defaultPrevented && !isImeComposing(e)) {
            e.preventDefault();
            if (!saving) void handleSave();
          }
          if (e.key === "Escape" && !saving) onClose();
        }}
        className="bg-surface-900 border border-surface-700 rounded px-2 focus-within:border-brand-600"
      >
        <label htmlFor="session-group-modal-field" className="sr-only">
          Group
        </label>
        <GroupCombobox
          id="session-group-modal-field"
          testId="session-group-modal-input"
          autoFocus
          inline
          showAllUntilEdited
          disabled={saving}
          openOnFocus={!refocusing}
          value={value}
          onChange={(v) => {
            setValue(v);
            setError(null);
          }}
          groups={groups}
        />
      </div>
      <p className="text-[12px] text-text-dim">
        Use <span className="font-mono">/</span> for hierarchy, for example{" "}
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

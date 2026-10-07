import { useState } from "react";
import { SidebarModal } from "./SidebarModal";
import { MODAL_CANCEL, MODAL_INPUT, MODAL_PRIMARY } from "./styles";

/** Renames the worktree directory and, when opted in, the git branch. */
export function WorkdirNameModal({
  title,
  currentBranch,
  onCancel,
  onSubmit,
}: {
  title: string;
  currentBranch: string | null;
  onCancel: () => void;
  onSubmit: (name: string, renameBranch: boolean) => Promise<{ ok: boolean; message?: string }>;
}) {
  const [name, setName] = useState("");
  const [renameBranch, setRenameBranch] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    if (busy) return;
    const trimmed = name.trim();
    if (!trimmed) {
      setError("Enter a new workdir name.");
      return;
    }
    setBusy(true);
    setError(null);
    const res = await onSubmit(trimmed, renameBranch);
    setBusy(false);
    if (!res.ok) setError(res.message ?? "Failed to edit the workdir name.");
  };

  return (
    <SidebarModal
      testId="workdir-modal"
      label="Edit workdir name"
      heading="Edit workdir name"
      title={title}
      subtitle={
        currentBranch && <div className="mt-1 text-[11px] text-text-dim font-mono">Current branch: {currentBranch}</div>
      }
      onCancel={onCancel}
    >
      <div className="px-4 py-3 flex flex-col gap-3">
        <input
          type="text"
          autoFocus
          aria-label="New workdir name"
          disabled={busy}
          value={name}
          onChange={(e) => {
            setName(e.target.value);
            setError(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              void submit();
            }
          }}
          placeholder="new-workdir-name"
          data-testid="workdir-modal-name"
          className={MODAL_INPUT}
        />
        <label className="flex items-center gap-2 text-sm text-text-secondary cursor-pointer">
          <input
            type="checkbox"
            disabled={busy}
            checked={renameBranch}
            onChange={(e) => setRenameBranch(e.target.checked)}
            data-testid="workdir-modal-rename-branch"
          />
          Also rename git branch
        </label>
        {error && (
          <div data-testid="workdir-modal-error" className="text-[11px] text-status-error">
            {error}
          </div>
        )}
      </div>
      <div className="px-4 py-3 border-t border-surface-700/40 flex justify-end gap-2">
        <button onClick={onCancel} className={MODAL_CANCEL}>
          Cancel
        </button>
        <button
          onClick={() => void submit()}
          disabled={busy}
          data-testid="workdir-modal-save"
          className={`${MODAL_PRIMARY} disabled:opacity-50`}
        >
          {busy ? "Saving…" : "Save"}
        </button>
      </div>
    </SidebarModal>
  );
}

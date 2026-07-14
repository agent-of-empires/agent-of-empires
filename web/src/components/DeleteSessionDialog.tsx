import { useCallback, useEffect, useRef, useState } from "react";
import type { DeleteSessionOptions } from "../lib/api";
import type { CleanupDefaults } from "../lib/types";

interface AffectedSession {
  id: string;
  title: string;
  isSandboxed: boolean;
}

interface Props {
  sessionTitle: string;
  branchName: string | null;
  hasManagedWorktree: boolean;
  isSandboxed: boolean;
  isScratch: boolean;
  cleanupDefaults: CleanupDefaults;
  /** When true (session.delete_to_trash), the dialog defaults to "Move to
   *  Trash" with a "Delete permanently" disclosure; when false it goes
   *  straight to the permanent-delete options. See #2489. */
  defaultToTrash: boolean;
  /** A workspace shares one worktree across all its sessions, and delete acts
   *  on the whole workspace. When this has more than one item the dialog uses
   *  workspace-shaped copy instead of presenting the action as single-session. */
  affectedSessions?: AffectedSession[];
  onConfirm: (options: DeleteSessionOptions) => Promise<void>;
  onTrash: () => Promise<void>;
  onCancel: () => void;
}

export function DeleteSessionDialog({
  sessionTitle,
  branchName,
  hasManagedWorktree,
  isSandboxed,
  isScratch,
  cleanupDefaults,
  defaultToTrash,
  affectedSessions,
  onConfirm,
  onTrash,
  onCancel,
}: Props) {
  const [deleteWorktree, setDeleteWorktree] = useState(hasManagedWorktree && cleanupDefaults.delete_worktree);
  const [forceDelete, setForceDelete] = useState(false);
  const [deleteBranch, setDeleteBranch] = useState(hasManagedWorktree && cleanupDefaults.delete_branch);
  const [deleteSandbox, setDeleteSandbox] = useState(isSandboxed && cleanupDefaults.delete_sandbox);
  // Scratch sessions default to remove. The user opts in to keep when they
  // realize mid-delete they want to rescue the files.
  const [keepScratch, setKeepScratch] = useState(false);
  const [deleting, setDeleting] = useState(false);
  // Permanent-delete mode. Off by default when trash-first is enabled; the
  // user reveals the destructive options via "Delete permanently". When
  // trash-first is disabled there is no trash step, so start permanent.
  const [permanent, setPermanent] = useState(!defaultToTrash);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  const hasOptions = hasManagedWorktree || isSandboxed || isScratch;
  const sessions = affectedSessions?.length ? affectedSessions : [{ id: "primary", title: sessionTitle, isSandboxed }];
  const isWorkspaceDelete = sessions.length > 1;
  const sandboxedSessionCount = sessions.filter((session) => session.isSandboxed).length;
  const sandboxedSessionLabel = sandboxedSessionCount === 1 ? "session" : "sessions";
  const worktreeDetail = branchName
    ? isWorkspaceDelete
      ? `Removes the workspace worktree for branch "${branchName}"`
      : `Removes worktree for branch "${branchName}"`
    : isWorkspaceDelete
      ? "Removes the workspace worktree"
      : undefined;
  const branchDetail = branchName
    ? isWorkspaceDelete
      ? `Removes the workspace branch "${branchName}"`
      : `Removes branch "${branchName}"`
    : undefined;
  const sandboxDetail = isWorkspaceDelete
    ? sandboxedSessionCount > 0 && sandboxedSessionCount < sessions.length
      ? `Removes Docker sandbox containers for ${sandboxedSessionCount} sandboxed ${sandboxedSessionLabel} in this workspace`
      : "Removes Docker sandbox containers for all sessions in this workspace"
    : "Removes the Docker sandbox container";
  const scratchDetail = isWorkspaceDelete
    ? "Leaves scratch directories on disk; session records are still removed"
    : "Leaves the scratch directory on disk; session record is still removed";

  const handleConfirm = useCallback(async () => {
    setDeleting(true);
    try {
      if (permanent) {
        await onConfirm({
          delete_worktree: deleteWorktree,
          delete_branch: deleteBranch,
          delete_sandbox: deleteSandbox,
          force_delete: forceDelete,
          keep_scratch: isScratch ? keepScratch : undefined,
        });
      } else {
        await onTrash();
      }
    } catch {
      setDeleting(false);
    }
  }, [permanent, onConfirm, onTrash, deleteWorktree, deleteBranch, deleteSandbox, forceDelete, isScratch, keepScratch]);

  // Capture the previously focused element on mount and restore focus on
  // unmount so keyboard users return to the trigger (the sidebar row /
  // context-menu item) instead of losing focus to document.body.
  useEffect(() => {
    previousFocusRef.current = document.activeElement as HTMLElement | null;
    confirmButtonRef.current?.focus();
    return () => {
      previousFocusRef.current?.focus?.();
    };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onCancel();
        return;
      }
      if (e.key === "Enter") {
        // Skip when focus is on an element that has its own Enter
        // semantics so we don't double-fire or override defaults:
        //   - native input/textarea: leave their own behavior alone
        //   - any button (including the Delete button itself): the
        //     browser already activates the focused button on Enter,
        //     so handling it here would call handleConfirm twice.
        const target = e.target as HTMLElement | null;
        if (target) {
          const tag = target.tagName;
          if (tag === "INPUT" || tag === "TEXTAREA" || tag === "BUTTON") return;
        }
        if (deleting) return;
        e.preventDefault();
        void handleConfirm();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, handleConfirm, deleting]);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="delete-session-dialog-title"
      data-testid="delete-session-dialog"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onCancel}
    >
      <div
        data-testid="delete-session-dialog-panel"
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[420px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="px-5 py-4 border-b border-surface-700">
          <h2 id="delete-session-dialog-title" className="text-sm font-semibold text-status-error">
            {isWorkspaceDelete ? "Delete Workspace" : "Delete Session"}
          </h2>
        </div>

        {/* Body */}
        <div className="px-5 py-4 space-y-3">
          {isWorkspaceDelete ? (
            <div className="space-y-2">
              <p className="text-[13px] text-text-secondary">
                {permanent ? "Permanently delete this workspace?" : "Move this workspace to Trash?"}
              </p>
              <p className="text-[12px] text-text-dim" data-testid="delete-session-affected-count">
                This affects all {sessions.length} sessions in this workspace.
              </p>
              <ul
                className="max-h-32 overflow-y-auto rounded-md border border-surface-700/60 bg-surface-900/40 p-2 space-y-1"
                data-testid="delete-session-affected-list"
              >
                {sessions.map((session) => (
                  <li key={session.id} className="font-mono text-[12px] text-text-secondary break-all">
                    {session.title}
                  </li>
                ))}
              </ul>
            </div>
          ) : (
            <p className="text-[13px] text-text-secondary">
              Delete <span className="font-mono text-text-primary break-all">{sessionTitle}</span>?
            </p>
          )}

          {/* When trash is the default, deleting moves the target to the
              Trash (restore later); this checkbox opts into erasing it now.
              When trash-first is off (or the session is already trashed),
              there is no checkbox and delete is always permanent. See #2489. */}
          {defaultToTrash && (
            <Checkbox
              checked={permanent}
              onChange={setPermanent}
              label="Delete permanently"
              detail={
                isWorkspaceDelete
                  ? "Skip the trash and erase now, including all transcripts. Off: move to Trash, restore later."
                  : "Skip the trash and erase now, including the transcript. Off: move to Trash, restore later."
              }
              testId="delete-session-permanent"
            />
          )}

          {permanent && hasOptions && (
            <div className="space-y-2 pt-1">
              {hasManagedWorktree && (
                <>
                  <Checkbox
                    checked={deleteWorktree}
                    onChange={setDeleteWorktree}
                    label="Delete worktree"
                    detail={worktreeDetail}
                    testId="delete-session-checkbox-worktree"
                  />
                  {deleteWorktree && (
                    <div className="pl-6">
                      <Checkbox
                        checked={forceDelete}
                        onChange={setForceDelete}
                        label="Force delete"
                        detail="Delete even if worktree has uncommitted changes"
                        testId="delete-session-checkbox-force"
                      />
                    </div>
                  )}
                  <Checkbox
                    checked={deleteBranch}
                    onChange={setDeleteBranch}
                    label="Delete branch"
                    detail={branchDetail}
                    testId="delete-session-checkbox-branch"
                  />
                </>
              )}
              {isSandboxed && (
                <Checkbox
                  checked={deleteSandbox}
                  onChange={setDeleteSandbox}
                  label={isWorkspaceDelete ? "Delete containers" : "Delete container"}
                  detail={sandboxDetail}
                  testId="delete-session-checkbox-sandbox"
                />
              )}
              {isScratch && (
                <Checkbox
                  checked={keepScratch}
                  onChange={setKeepScratch}
                  label={isWorkspaceDelete ? "Keep scratch directories" : "Keep scratch directory"}
                  detail={scratchDetail}
                  testId="delete-session-checkbox-keep-scratch"
                />
              )}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex justify-end gap-3 px-5 py-3 border-t border-surface-700">
          <button
            onClick={onCancel}
            disabled={deleting}
            className="px-3 py-1.5 text-sm text-text-secondary hover:text-text-primary rounded-md hover:bg-surface-700/50 cursor-pointer transition-colors disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            ref={confirmButtonRef}
            onClick={handleConfirm}
            disabled={deleting}
            data-testid="delete-session-confirm"
            className="px-3 py-1.5 text-sm text-white rounded-md cursor-pointer transition-colors disabled:opacity-50 flex items-center gap-2 bg-status-error/90 hover:bg-status-error"
          >
            {deleting && (
              <svg className="animate-spin h-3.5 w-3.5" viewBox="0 0 24 24">
                <circle
                  className="opacity-25"
                  cx="12"
                  cy="12"
                  r="10"
                  stroke="currentColor"
                  strokeWidth="4"
                  fill="none"
                />
                <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
              </svg>
            )}
            {deleting ? "Deleting..." : "Delete"}
          </button>
        </div>
      </div>
    </div>
  );
}

function Checkbox({
  checked,
  onChange,
  label,
  detail,
  testId,
}: {
  checked: boolean;
  onChange: (val: boolean) => void;
  label: string;
  detail?: string;
  testId?: string;
}) {
  return (
    <label
      className="flex items-start gap-2.5 cursor-pointer group"
      data-testid={testId}
      data-checked={checked ? "true" : "false"}
    >
      {/*
        Native checkbox input drives state so the control is reachable
        by Tab and toggles with Space, matching the platform contract
        for "Keep scratch directory" and the other checkboxes here.
        The visible square below is a styled affordance that mirrors
        the input's checked state via Tailwind's `peer` selector; the
        input itself is visually hidden but not aria-hidden.
      */}
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        aria-label={label}
        className="peer sr-only"
      />
      <span
        aria-hidden="true"
        className={`mt-0.5 w-4 h-4 rounded border flex items-center justify-center shrink-0 transition-colors peer-focus-visible:outline peer-focus-visible:outline-2 peer-focus-visible:outline-offset-2 peer-focus-visible:outline-status-error ${
          checked ? "bg-status-error border-status-error" : "border-surface-600 group-hover:border-surface-500"
        }`}
      >
        {checked && (
          <svg width="10" height="10" viewBox="0 0 10 10" fill="none">
            <path d="M2 5L4 7L8 3" stroke="white" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        )}
      </span>
      <span className="flex flex-col min-w-0">
        <span className="text-[13px] text-text-secondary group-hover:text-text-primary transition-colors">{label}</span>
        {detail && <span className="text-[12px] text-text-dim">{detail}</span>}
      </span>
    </label>
  );
}

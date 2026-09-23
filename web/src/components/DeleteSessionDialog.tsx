import { useCallback, useRef, useState } from "react";
import type { DeleteSessionOptions } from "../lib/api";
import type { CleanupDefaults } from "../lib/types";
import { CancelButton, ConfirmButton, DANGER_BUTTON, Dialog } from "./Dialog";
import { useBusyAction, useConfirmKeys, useDialogFocus } from "./dialogHooks";

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
  /** session.delete_to_trash: default to Trash with a "Delete permanently" opt-in. */
  defaultToTrash: boolean;
  /** Sessions sharing the workspace worktree; more than one switches to workspace copy. */
  affectedSessions?: AffectedSession[];
  /** Titles of unselected sessions using the worktree, which the server then keeps with its branch. */
  worktreeSharedWith?: string[];
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
  worktreeSharedWith = [],
  onConfirm,
  onTrash,
  onCancel,
}: Props) {
  const [deleteWorktree, setDeleteWorktree] = useState(hasManagedWorktree && cleanupDefaults.delete_worktree);
  const [forceDelete, setForceDelete] = useState(false);
  const [deleteBranch, setDeleteBranch] = useState(hasManagedWorktree && cleanupDefaults.delete_branch);
  const [deleteSandbox, setDeleteSandbox] = useState(isSandboxed && cleanupDefaults.delete_sandbox);
  const [keepScratch, setKeepScratch] = useState(false);
  const [permanent, setPermanent] = useState(!defaultToTrash);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);

  const hasOptions = hasManagedWorktree || isSandboxed || isScratch;
  const worktreeShared = hasManagedWorktree && worktreeSharedWith.length > 0;
  const sessions = affectedSessions?.length ? affectedSessions : [{ id: "primary", title: sessionTitle, isSandboxed }];
  const workspace = sessions.length > 1;
  const sandboxedCount = sessions.filter((session) => session.isSandboxed).length;
  const worktreeDetail = branchName
    ? `Removes ${workspace ? "the workspace worktree" : "worktree"} for branch "${branchName}"`
    : workspace
      ? "Removes the workspace worktree"
      : undefined;
  const branchDetail = branchName
    ? `Removes ${workspace ? "the workspace branch" : "branch"} "${branchName}"`
    : undefined;
  // Hedged ("any private agent store") because pre-v027 sessions share one store.
  const sandboxDetail = !workspace
    ? "Removes the Docker sandbox container and any private agent store it has (including the saved agent login)"
    : sandboxedCount > 0 && sandboxedCount < sessions.length
      ? `Removes Docker sandbox containers, and any private agent store, for ${sandboxedCount} sandboxed ${sandboxedCount === 1 ? "session" : "sessions"} in this workspace`
      : "Removes Docker sandbox containers, and any private agent store, for all sessions in this workspace";
  const scratchDetail = workspace
    ? "Leaves scratch directories on disk; session records are still removed"
    : "Leaves the scratch directory on disk; session record is still removed";

  const confirm = useCallback(
    () =>
      permanent
        ? onConfirm({
            delete_worktree: deleteWorktree && !worktreeShared,
            delete_branch: deleteBranch && !worktreeShared,
            delete_sandbox: deleteSandbox,
            force_delete: forceDelete,
            keep_scratch: isScratch ? keepScratch : undefined,
          })
        : onTrash(),
    [
      permanent,
      onConfirm,
      onTrash,
      deleteWorktree,
      deleteBranch,
      worktreeShared,
      deleteSandbox,
      forceDelete,
      isScratch,
      keepScratch,
    ],
  );
  const [deleting, handleConfirm] = useBusyAction(confirm);
  useDialogFocus(confirmButtonRef);
  useConfirmKeys(onCancel, handleConfirm, deleting);

  return (
    <Dialog
      id="delete-session-dialog"
      panelTestId="delete-session-dialog-panel"
      title={workspace ? "Delete Workspace" : "Delete Session"}
      titleClassName="text-status-error"
      bodyClassName="px-5 py-4 space-y-3"
      onDismiss={onCancel}
      footer={
        <>
          <CancelButton onClick={onCancel} disabled={deleting} />
          <ConfirmButton
            buttonRef={confirmButtonRef}
            onClick={handleConfirm}
            busy={deleting}
            testId="delete-session-confirm"
            className={DANGER_BUTTON}
          >
            {deleting ? "Deleting..." : "Delete"}
          </ConfirmButton>
        </>
      }
    >
      {workspace ? (
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

      {defaultToTrash && (
        <Checkbox
          checked={permanent}
          onChange={setPermanent}
          label="Delete permanently"
          detail={`Skip the trash and erase now, including ${workspace ? "all transcripts" : "the transcript"}. Off: move to Trash, restore later.`}
          testId="delete-session-permanent"
        />
      )}

      {permanent && hasOptions && (
        <div className="space-y-2 pt-1">
          {worktreeShared && (
            <p className="text-[12px] text-text-dim" data-testid="delete-session-shared-worktree">
              Worktree and branch are kept:{" "}
              {worktreeSharedWith.length === 1
                ? `"${worktreeSharedWith[0]}" still uses it`
                : `${worktreeSharedWith.length} other sessions still use it`}
              .
            </p>
          )}
          {hasManagedWorktree && !worktreeShared && (
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
              label={workspace ? "Delete containers" : "Delete container"}
              detail={sandboxDetail}
              testId="delete-session-checkbox-sandbox"
            />
          )}
          {isScratch && (
            <Checkbox
              checked={keepScratch}
              onChange={setKeepScratch}
              label={workspace ? "Keep scratch directories" : "Keep scratch directory"}
              detail={scratchDetail}
              testId="delete-session-checkbox-keep-scratch"
            />
          )}
        </div>
      )}
    </Dialog>
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
      {/* The native input stays focusable (sr-only, not aria-hidden); the span mirrors it via `peer`. */}
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

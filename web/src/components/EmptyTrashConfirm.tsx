import { useCallback, useRef } from "react";
import { CancelButton, ConfirmButton, DANGER_BUTTON, Dialog } from "./Dialog";
import { useConfirmKeys, useDialogFocus } from "./dialogHooks";

export function EmptyTrashConfirm({
  sessionCount,
  onConfirm,
  onCancel,
}: {
  sessionCount: number;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  // A ref, not state, so an Enter+click double fire is blocked before a re-render.
  const firedRef = useRef(false);
  const confirm = useCallback(() => {
    if (firedRef.current) return;
    firedRef.current = true;
    onConfirm();
  }, [onConfirm]);
  useDialogFocus(confirmButtonRef);
  useConfirmKeys(onCancel, confirm);

  return (
    <Dialog
      id="empty-trash-dialog"
      title="Empty Trash"
      titleClassName="text-status-error"
      onDismiss={onCancel}
      footer={
        <>
          <CancelButton onClick={onCancel} />
          <ConfirmButton
            buttonRef={confirmButtonRef}
            onClick={confirm}
            testId="empty-trash-confirm"
            className={DANGER_BUTTON}
          >
            Empty Trash
          </ConfirmButton>
        </>
      }
    >
      <p className="text-[13px] text-text-secondary">
        Permanently delete {sessionCount} trashed {sessionCount === 1 ? "session" : "sessions"}? This cannot be undone.
      </p>
    </Dialog>
  );
}

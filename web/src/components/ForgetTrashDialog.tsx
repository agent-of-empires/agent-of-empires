import { useRef } from "react";
import { CancelButton, ConfirmButton, DANGER_BUTTON, Dialog } from "./Dialog";
import { useBusyAction, useConfirmKeys, useDialogFocus } from "./dialogHooks";

interface Props {
  sessionTitle: string;
  profile: string;
  onConfirm: () => Promise<void>;
  onCancel: () => void;
}

export function ForgetTrashDialog({ sessionTitle, profile, onConfirm, onCancel }: Props) {
  const confirmRef = useRef<HTMLButtonElement | null>(null);
  const [busy, confirm] = useBusyAction(onConfirm);
  useDialogFocus(confirmRef);
  useConfirmKeys(onCancel, confirm, busy);

  return (
    <Dialog
      id="forget-trash-dialog"
      title="Forget Trash Record"
      onDismiss={onCancel}
      footer={
        <>
          <CancelButton onClick={onCancel} disabled={busy} />
          <ConfirmButton buttonRef={confirmRef} onClick={confirm} busy={busy} className={DANGER_BUTTON}>
            Forget record
          </ConfirmButton>
        </>
      }
    >
      <p>
        Forget <strong>{sessionTitle}</strong> in profile <strong>{profile}</strong> without deleting its files?
      </p>
      <p className="mt-3 text-text-secondary">
        AoE keeps cleanup ownership of its runtime and storage resources. This action does not stop processes, run
        delete hooks, or remove files.
      </p>
      <p className="mt-3 text-text-secondary">
        Retained ownership blocks profile rename/deletion. You cannot restore this session through Trash afterward.
      </p>
    </Dialog>
  );
}

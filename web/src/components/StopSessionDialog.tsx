import { useRef } from "react";
import { BRAND_BUTTON, CancelButton, ConfirmButton, Dialog } from "./Dialog";
import { useBusyAction, useConfirmKeys, useDialogFocus } from "./dialogHooks";

interface Props {
  sessionTitle: string;
  onConfirm: () => Promise<void>;
  onCancel: () => void;
}

export function StopSessionDialog({ sessionTitle, onConfirm, onCancel }: Props) {
  const [stopping, handleConfirm] = useBusyAction(onConfirm);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  useDialogFocus(confirmButtonRef);
  useConfirmKeys(onCancel, handleConfirm, stopping);

  return (
    <Dialog
      id="stop-session-dialog"
      title="Stop Session"
      onDismiss={onCancel}
      footer={
        <>
          <CancelButton onClick={onCancel} disabled={stopping} />
          <ConfirmButton buttonRef={confirmButtonRef} onClick={handleConfirm} busy={stopping} className={BRAND_BUTTON}>
            {stopping ? "Stopping..." : "Stop"}
          </ConfirmButton>
        </>
      }
    >
      <p className="text-[13px] text-text-secondary">
        Are you sure you want to stop <span className="font-mono text-text-primary">{sessionTitle}</span>? The agent
        stops, but the session is kept and can be resumed later.
      </p>
    </Dialog>
  );
}

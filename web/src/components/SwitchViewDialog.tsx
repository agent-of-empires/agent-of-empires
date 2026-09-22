import { useRef } from "react";
import { switchViewCopy } from "../lib/acpKeepContext";
import { BRAND_BUTTON, CancelButton, ConfirmButton, Dialog } from "./Dialog";
import { useBusyAction, useConfirmKeys, useDialogFocus } from "./dialogHooks";

interface Props {
  sessionTitle: string;
  /** true switches terminal to structured, false the reverse. */
  toStructured: boolean;
  /** Whether the pairing preserves the conversation; drives the copy. */
  keepsContext: boolean;
  onConfirm: () => Promise<void>;
  onCancel: () => void;
}

export function SwitchViewDialog({ sessionTitle, toStructured, keepsContext, onConfirm, onCancel }: Props) {
  const [switching, handleConfirm] = useBusyAction(onConfirm);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  const copy = switchViewCopy(toStructured, keepsContext);
  useDialogFocus(confirmButtonRef);
  useConfirmKeys(onCancel, handleConfirm, switching);

  return (
    <Dialog
      id="switch-view-dialog"
      title={copy.title}
      onDismiss={onCancel}
      footer={
        <>
          <CancelButton onClick={onCancel} disabled={switching} />
          <ConfirmButton
            buttonRef={confirmButtonRef}
            onClick={handleConfirm}
            busy={switching}
            testId="switch-view-confirm"
            className={BRAND_BUTTON}
          >
            {switching ? "Switching..." : copy.confirmLabel}
          </ConfirmButton>
        </>
      }
    >
      <p className="text-[13px] text-text-secondary">
        <span className="font-mono text-text-primary">{sessionTitle}</span>: {copy.body}
      </p>
    </Dialog>
  );
}

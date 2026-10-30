import { useRef, useState } from "react";
import { recreateSessionContainer } from "../../lib/api";
import { reportInfo } from "../../lib/toastBus";
import { CancelButton, ConfirmButton, DANGER_BUTTON, Dialog } from "../Dialog";
import { useConfirmKeys, useDialogFocus } from "../dialogHooks";

/** Confirms the one action that discards a sandboxed session's container; it stays open on a refusal. */
export function RecreateContainerDialog({
  sessionId,
  sessionTitle,
  onClose,
}: {
  sessionId: string;
  sessionTitle: string;
  onClose: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // A ref, not state, so an Enter+click double fire sends one request.
  const sendingRef = useRef(false);
  const confirm = async () => {
    if (sendingRef.current) return;
    sendingRef.current = true;
    setBusy(true);
    setError(null);
    const result = await recreateSessionContainer(sessionId);
    sendingRef.current = false;
    setBusy(false);
    if (result.ok) {
      reportInfo(`Restarting ${sessionTitle} on a new container…`);
      onClose();
    } else {
      setError(result.message ?? "please try again.");
    }
  };
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  useDialogFocus(confirmButtonRef);
  useConfirmKeys(onClose, () => void confirm(), busy);

  return (
    <Dialog
      id="recreate-container-dialog"
      title="Recreate container"
      onDismiss={onClose}
      footer={
        <>
          <CancelButton onClick={onClose} disabled={busy} />
          <ConfirmButton
            buttonRef={confirmButtonRef}
            onClick={() => void confirm()}
            busy={busy}
            testId="recreate-container-confirm"
            className={DANGER_BUTTON}
          >
            {busy ? "Recreating…" : "Recreate container"}
          </ConfirmButton>
        </>
      }
    >
      <p className="text-[13px] text-text-subtle">
        Recreate this Session&apos;s container from the current image? Files outside the Working directory and the
        Sandbox&apos;s mounted folders are lost, including anything installed inside the container.
      </p>
      <p className="mt-2 text-[13px] text-text-subtle">
        <span className="font-mono text-text-primary">{sessionTitle}</span> restarts: a running turn stops, and the
        conversation resumes as it does on a restart.
      </p>
      {error && (
        <p role="alert" className="mt-2 text-[13px] text-text-primary">
          Couldn&apos;t recreate the container: {error}
        </p>
      )}
    </Dialog>
  );
}

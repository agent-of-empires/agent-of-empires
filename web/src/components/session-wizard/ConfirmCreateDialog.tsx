import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { useFocusTrap } from "../../hooks/useFocusTrap";
import { CancelButton, ConfirmButton, Dialog } from "../Dialog";

interface Props {
  /** Prefix for the `-dialog` and `-proceed` test ids. */
  id: string;
  title: string;
  confirmLabel: string;
  onConfirm: () => Promise<void> | void;
  onCancel: () => void;
  children: ReactNode;
}

/** Shared modal shell for confirmations that pause a session create. */
export function ConfirmCreateDialog({ id, title, confirmLabel, onConfirm, onCancel, children }: Props) {
  const [confirming, setConfirming] = useState(false);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);
  // Without its own trap, Tab leaves for the wizard behind it, whose trap then pulls focus in.
  useFocusTrap(() => dialogRef.current);

  const handleConfirm = useCallback(async () => {
    setConfirming(true);
    try {
      await onConfirm();
    } catch {
      setConfirming(false);
    }
  }, [onConfirm]);

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
        const tag = (e.target as HTMLElement | null)?.tagName;
        if (tag === "INPUT" || tag === "TEXTAREA" || tag === "BUTTON") return;
        if (confirming) return;
        e.preventDefault();
        void handleConfirm();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, handleConfirm, confirming]);

  return (
    <Dialog
      id={`${id}-dialog`}
      title={title}
      titleClassName="text-status-warning"
      rootRef={dialogRef}
      bodyClassName="space-y-3"
      onDismiss={onCancel}
      footer={
        <>
          <CancelButton onClick={onCancel} disabled={confirming} />
          <ConfirmButton
            buttonRef={confirmButtonRef}
            onClick={handleConfirm}
            busy={confirming}
            testId={`${id}-proceed`}
          >
            {confirming ? "Creating..." : confirmLabel}
          </ConfirmButton>
        </>
      }
    >
      {children}
    </Dialog>
  );
}

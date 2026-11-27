import { useEffect, type ReactNode, type Ref } from "react";
import { CAPTION } from "../../lib/surfaces";
import { CancelButton, CloseButton, ConfirmButton, ModalFrame } from "../Dialog";
import { DIALOG_BODY, DIALOG_FOOTER } from "../../lib/surfaces";

interface Props {
  testId: string;
  ariaLabel: string;
  header: ReactNode;
  closeTestId: string;
  /** The Close button's accessible name. */
  closeLabel?: string;
  onClose: () => void;
  /** While busy, Escape, the backdrop and Close do nothing. */
  busy?: boolean;
  /** Set false when the caller owns Escape. */
  closeOnEscape?: boolean;
  rootRef?: Ref<HTMLDivElement>;
  /** Desktop size, as `md:` classes. */
  panelClassName?: string;
  /** Rendered outside the panel, e.g. a lightbox. */
  overlay?: ReactNode;
  /** The footer's buttons. */
  actions?: ReactNode;
  children: ReactNode;
}

/** Backdrop, panel, header-with-Close and footer shared by the plugin modals. */
export function PluginModal({
  testId,
  ariaLabel,
  header,
  closeTestId,
  closeLabel,
  onClose,
  busy = false,
  closeOnEscape = true,
  rootRef,
  panelClassName = "md:w-[32rem] md:max-h-[80vh]",
  overlay,
  actions,
  children,
}: Props) {
  const closeIfIdle = () => {
    if (!busy) onClose();
  };

  useEffect(() => {
    if (!closeOnEscape) return;
    const onKey = (e: KeyboardEvent) => {
      if (!busy && e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose, closeOnEscape]);

  return (
    <ModalFrame
      rootProps={{ ref: rootRef, role: "dialog", "aria-modal": true, "aria-label": ariaLabel, "data-testid": testId }}
      panelClassName={panelClassName}
      onBackdrop={busy ? undefined : onClose}
      outside={overlay}
    >
      <div className="flex items-start gap-3 px-5 max-md:px-4 pt-4 pb-1 max-md:pt-2 text-[15px] md:text-sm">
        <div className="flex-1 min-w-0">{header}</div>
        <CloseButton testId={closeTestId} label={closeLabel} disabled={busy} onClick={closeIfIdle} />
      </div>
      <div className={`${DIALOG_BODY} text-[15px] md:text-sm`}>{children}</div>
      {actions && <div className={DIALOG_FOOTER}>{actions}</div>}
    </ModalFrame>
  );
}

/** A labelled disclosure block: uppercase caption over its content. */
export function ModalSection({
  label,
  warn = false,
  testId,
  children,
}: {
  label: string;
  warn?: boolean;
  testId?: string;
  children: ReactNode;
}) {
  const color = warn ? "text-status-warning" : "text-text-dim";
  return (
    <div className="mb-3" data-testid={testId}>
      <p className={`${CAPTION} !px-0 !mb-1 ${warn ? "!text-status-warning" : ""}`}>{label}</p>
      {typeof children === "string" ? <p className={`text-[13px] md:text-xs ${color}`}>{children}</p> : children}
    </div>
  );
}

export function BuildSteps({ steps, testId }: { steps: string[]; testId: string }) {
  if (steps.length === 0) return null;
  return (
    <ModalSection label="Build commands (run as you, unsandboxed)" warn testId={testId}>
      <ul className="space-y-0.5">
        {steps.map((step, i) => (
          <li key={i} className="font-mono text-[11px] text-text-dim">
            $ {step}
          </li>
        ))}
      </ul>
    </ModalSection>
  );
}

/** The footer's cancel/confirm pair; both disable while `busy`. */
export function ModalActions(p: {
  busy: boolean;
  cancelLabel: string;
  cancelTestId: string;
  onCancel: () => void;
  confirmLabel: string;
  confirmTestId: string;
  onConfirm: () => void;
}) {
  return (
    <>
      <CancelButton label={p.cancelLabel} testId={p.cancelTestId} disabled={p.busy} onClick={p.onCancel} />
      <ConfirmButton testId={p.confirmTestId} disabled={p.busy} onClick={p.onConfirm}>
        {p.confirmLabel}
      </ConfirmButton>
    </>
  );
}

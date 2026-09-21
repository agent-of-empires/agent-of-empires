import { useEffect, type ReactNode } from "react";

interface Props {
  testId: string;
  ariaLabel: string;
  header: ReactNode;
  closeTestId: string;
  onClose: () => void;
  /** While busy, Escape, the backdrop and Close do nothing. */
  busy?: boolean;
  /** Set false when the caller owns Escape. */
  closeOnEscape?: boolean;
  /** Rendered outside the panel, e.g. a lightbox. */
  overlay?: ReactNode;
  children: ReactNode;
}

/** Backdrop, panel and header-with-Close shared by the plugin modals. */
export function PluginModal({
  testId,
  ariaLabel,
  header,
  closeTestId,
  onClose,
  busy = false,
  closeOnEscape = true,
  overlay,
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
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={ariaLabel}
      onClick={closeIfIdle}
      data-testid={testId}
    >
      <div
        className="max-h-[80vh] w-full max-w-lg overflow-auto rounded border border-surface-700 bg-surface-900 p-4 text-sm"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-start justify-between gap-3">
          {header}
          <button
            type="button"
            className="rounded border border-surface-700 px-2 py-0.5 text-xs hover:bg-surface-800 disabled:opacity-50"
            disabled={busy}
            onClick={closeIfIdle}
            data-testid={closeTestId}
          >
            Close
          </button>
        </div>
        {children}
      </div>
      {overlay}
    </div>
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
      <p className={`mb-1 text-[11px] font-semibold uppercase tracking-wide ${color}`}>{label}</p>
      {typeof children === "string" ? <p className={`text-xs ${color}`}>{children}</p> : children}
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

const CANCEL = "rounded border border-surface-700 px-3 py-1 text-xs hover:bg-surface-800 disabled:opacity-50";
const CONFIRM = "rounded bg-brand-600 px-3 py-1 text-xs font-medium text-white hover:bg-brand-500 disabled:opacity-50";

/** The modal's trailing cancel/confirm pair; both go flat while `busy`. */
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
    <div className="flex justify-end gap-2">
      <button type="button" className={CANCEL} disabled={p.busy} onClick={p.onCancel} data-testid={p.cancelTestId}>
        {p.cancelLabel}
      </button>
      <button type="button" className={CONFIRM} disabled={p.busy} onClick={p.onConfirm} data-testid={p.confirmTestId}>
        {p.confirmLabel}
      </button>
    </div>
  );
}

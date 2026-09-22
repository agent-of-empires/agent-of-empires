import type { ReactNode, Ref } from "react";

export function Spinner() {
  return (
    <svg className="animate-spin h-3.5 w-3.5" viewBox="0 0 24 24">
      <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" />
      <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
    </svg>
  );
}

export function Dialog({
  id,
  title,
  titleClassName = "text-text-primary",
  panelTestId,
  bodyClassName = "px-5 py-4",
  onDismiss,
  footer,
  children,
}: {
  id: string;
  title: ReactNode;
  titleClassName?: string;
  panelTestId?: string;
  bodyClassName?: string;
  onDismiss: () => void;
  footer: ReactNode;
  children: ReactNode;
}) {
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby={`${id}-title`}
      data-testid={id}
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onDismiss}
    >
      <div
        data-testid={panelTestId}
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[420px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-surface-700">
          <h2 id={`${id}-title`} className={`text-sm font-semibold ${titleClassName}`}>
            {title}
          </h2>
        </div>
        <div className={bodyClassName}>{children}</div>
        <div className="flex justify-end gap-3 px-5 py-3 border-t border-surface-700">{footer}</div>
      </div>
    </div>
  );
}

export function CancelButton({ onClick, disabled }: { onClick: () => void; disabled?: boolean }) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className="px-3 py-1.5 text-sm text-text-secondary hover:text-text-primary rounded-md hover:bg-surface-700/50 cursor-pointer transition-colors disabled:opacity-50"
    >
      Cancel
    </button>
  );
}

export const DANGER_BUTTON = "text-white bg-status-error/90 hover:bg-status-error";
export const BRAND_BUTTON = "text-surface-950 bg-brand-600 hover:bg-brand-500";

export function ConfirmButton({
  buttonRef,
  onClick,
  busy = false,
  testId,
  className,
  children,
}: {
  buttonRef?: Ref<HTMLButtonElement>;
  onClick: () => void;
  busy?: boolean;
  testId?: string;
  className: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      ref={buttonRef}
      onClick={onClick}
      disabled={busy}
      data-testid={testId}
      className={`px-3 py-1.5 text-sm rounded-md cursor-pointer transition-colors disabled:opacity-50 flex items-center gap-2 ${className}`}
    >
      {busy && <Spinner />}
      {children}
    </button>
  );
}

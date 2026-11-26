import type { HTMLAttributes, MouseEvent, ReactNode, Ref } from "react";
import { X } from "lucide-react";
import {
  BACKDROP,
  BUTTON_DANGER,
  BUTTON_GHOST,
  BUTTON_PRIMARY,
  DIALOG_BODY,
  DIALOG_FOOTER,
  SHEET,
  SHEET_HANDLE,
} from "../lib/surfaces";

export function Spinner() {
  return (
    <svg className="animate-spin h-3.5 w-3.5" viewBox="0 0 24 24" aria-hidden="true">
      <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" />
      <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
    </svg>
  );
}

// SHEET on phones, DIALOG from md up. Tailwind only sees literal classes, so DIALOG's are spelled out under md:.
const PANEL = `${SHEET} relative w-full flex flex-col max-h-[calc(100dvh-env(safe-area-inset-top))] max-md:pb-[env(safe-area-inset-bottom)] md:rounded-2xl md:border md:border-surface-700/60 md:bg-surface-850 md:shadow-2xl animate-slide-up`;

type DivProps = HTMLAttributes<HTMLDivElement> & { ref?: Ref<HTMLDivElement>; "data-testid"?: string };

/**
 * The dimmed overlay and panel every modal shares: centered on desktop, a bottom sheet with a grab handle on
 * phones. The dialog role goes on `rootProps`, or on `panelProps` when the panel is what callers measure.
 */
export function ModalFrame({
  rootProps,
  panelProps,
  panelClassName = "md:w-[420px] md:max-h-[90vh]",
  layer = "z-50",
  onBackdrop,
  dismissOn = "click",
  outside,
  children,
}: {
  rootProps?: DivProps;
  panelProps?: DivProps;
  /** Desktop size, as `md:` classes. */
  panelClassName?: string;
  layer?: string;
  /** Omitted while the modal can't be dismissed. */
  onBackdrop?: () => void;
  /** `mousedown` keeps a text selection dragged out of a field from dismissing. */
  dismissOn?: "click" | "mousedown";
  /** Rendered inside the overlay after the panel, such as a lightbox. */
  outside?: ReactNode;
  children: ReactNode;
}) {
  const onBackdropEvent = (e: MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget) onBackdrop?.();
  };
  return (
    <div
      {...rootProps}
      className={`fixed inset-0 ${layer} ${BACKDROP} flex items-end md:items-center md:justify-center md:p-4 animate-fade-in`}
      onClick={dismissOn === "click" ? onBackdropEvent : undefined}
      onMouseDown={dismissOn === "mousedown" ? onBackdropEvent : undefined}
    >
      <div
        {...panelProps}
        className={`${PANEL} ${panelClassName}`}
        // Clicks inside never reach the overlay or whatever the dialog is rendered within.
        onClick={(e) => e.stopPropagation()}
      >
        <div className={`${SHEET_HANDLE} md:hidden`} aria-hidden="true" />
        {children}
      </div>
      {outside}
    </div>
  );
}

/** A dialog's title row; `end` holds trailing controls such as a Close button. */
export function DialogHeader({
  titleId,
  title,
  titleClassName = "text-text-primary",
  as: Heading = "h2",
  end,
}: {
  titleId?: string;
  title: ReactNode;
  titleClassName?: string;
  as?: "h1" | "h2";
  end?: ReactNode;
}) {
  return (
    <div className="flex items-center gap-2 min-h-12 px-5 max-md:px-4 pt-3 pb-1 max-md:pt-1">
      <Heading
        id={titleId}
        className={`flex-1 min-w-0 flex items-center gap-2 text-[16px] md:text-[15px] font-semibold ${titleClassName}`}
      >
        {title}
      </Heading>
      {end}
    </div>
  );
}

/** The header's dismiss control, 44px to the touch on phones. */
export function CloseButton({
  onClick,
  buttonRef,
  disabled,
  testId,
  label = "Close",
}: {
  onClick: () => void;
  buttonRef?: Ref<HTMLButtonElement>;
  disabled?: boolean;
  testId?: string;
  label?: string;
}) {
  return (
    <button
      type="button"
      ref={buttonRef}
      onClick={onClick}
      disabled={disabled}
      data-testid={testId}
      aria-label={label}
      className="-mr-2 shrink-0 inline-flex items-center justify-center h-8 w-8 max-md:h-11 max-md:w-11 rounded-lg text-text-dim hover:text-text-primary hover:bg-surface-700/50 cursor-pointer transition-colors disabled:opacity-40 disabled:cursor-not-allowed focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand-500"
    >
      <X className="h-4 w-4" aria-hidden="true" />
    </button>
  );
}

export function Dialog({
  id,
  testId = id,
  title,
  titleClassName,
  ariaLabel,
  headerEnd,
  panelTestId,
  panelClassName,
  bodyClassName = "",
  describedBy = `${id}-desc`,
  rootRef,
  rootTabIndex,
  layer,
  onDismiss,
  dismissOn,
  footer,
  outside,
  children,
}: {
  id: string;
  /** Defaults to `id`. */
  testId?: string;
  title: ReactNode;
  titleClassName?: string;
  /** Names the dialog instead of its title. */
  ariaLabel?: string;
  headerEnd?: ReactNode;
  panelTestId?: string;
  /** Desktop size, as `md:` classes. */
  panelClassName?: string;
  /** Added to the scrolling body. */
  bodyClassName?: string;
  /** Element id for aria-describedby: the body by default, a prompt id when the body has controls, or false. */
  describedBy?: string | false;
  rootRef?: Ref<HTMLDivElement>;
  rootTabIndex?: number;
  layer?: string;
  /** Backdrop dismissal; omitted while the dialog can't be dismissed. */
  onDismiss?: () => void;
  dismissOn?: "click" | "mousedown";
  footer?: ReactNode;
  outside?: ReactNode;
  children: ReactNode;
}) {
  return (
    <ModalFrame
      rootProps={{
        ref: rootRef,
        tabIndex: rootTabIndex,
        role: "dialog",
        "aria-modal": true,
        "aria-label": ariaLabel,
        "aria-labelledby": ariaLabel ? undefined : `${id}-title`,
        "aria-describedby": describedBy || undefined,
        "data-testid": testId,
      }}
      panelProps={{ "data-testid": panelTestId }}
      panelClassName={panelClassName}
      layer={layer}
      onBackdrop={onDismiss}
      dismissOn={dismissOn}
      outside={outside}
    >
      <DialogHeader titleId={`${id}-title`} title={title} titleClassName={titleClassName} end={headerEnd} />
      <div id={`${id}-desc`} className={`${DIALOG_BODY} ${bodyClassName}`}>
        {children}
      </div>
      {footer && <div className={DIALOG_FOOTER}>{footer}</div>}
    </ModalFrame>
  );
}

export function CancelButton({
  onClick,
  disabled,
  label = "Cancel",
  testId,
  buttonRef,
}: {
  onClick: () => void;
  disabled?: boolean;
  label?: string;
  testId?: string;
  buttonRef?: Ref<HTMLButtonElement>;
}) {
  return (
    <button
      type="button"
      ref={buttonRef}
      onClick={onClick}
      disabled={disabled}
      data-testid={testId}
      className={BUTTON_GHOST}
    >
      {label}
    </button>
  );
}

export function ConfirmButton({
  buttonRef,
  onClick,
  busy = false,
  disabled = false,
  type = "button",
  form,
  testId,
  tone = "brand",
  children,
}: {
  buttonRef?: Ref<HTMLButtonElement>;
  onClick?: () => void;
  busy?: boolean;
  disabled?: boolean;
  type?: "button" | "submit";
  /** Id of the form a submit button sits outside of. */
  form?: string;
  testId?: string;
  /** `danger` for an action that removes or stops something. */
  tone?: "brand" | "danger";
  children: ReactNode;
}) {
  return (
    <button
      type={type}
      form={form}
      ref={buttonRef}
      onClick={onClick}
      disabled={busy || disabled}
      data-testid={testId}
      className={tone === "danger" ? BUTTON_DANGER : BUTTON_PRIMARY}
    >
      {busy && <Spinner />}
      {children}
    </button>
  );
}

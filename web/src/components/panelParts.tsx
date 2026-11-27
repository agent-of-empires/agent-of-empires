// Components the review dock, Settings panels and in-thread cards share; their classes are in panelClasses.

import type { ReactNode } from "react";

import { CAPTION } from "../lib/surfaces";

/** A section's caption with an optional line under it, as Settings captions its groups. */
export function SectionHeading({
  title,
  description,
  as: Tag = "h3",
}: {
  title: ReactNode;
  description?: ReactNode;
  as?: "h3" | "h4";
}) {
  return (
    <div className="mb-2">
      <Tag className={description ? CAPTION.replace("mb-2", "mb-1") : CAPTION.replace("mb-2", "mb-0")}>{title}</Tag>
      {description && <p className="px-1 text-[13px] leading-normal text-text-dim">{description}</p>}
    </div>
  );
}

const NOTICE_TONE = {
  error: "border-status-error/40 bg-status-error/10 text-status-error-text",
  warning: "border-status-warning/40 bg-status-warning/10 text-status-warning",
  info: "border-surface-700/50 bg-surface-800 text-text-secondary",
} as const;

/** A tinted box for a failure, a caution or a note inside a panel or card. */
export function Notice({
  tone,
  title,
  children,
  testId,
  role,
  className = "",
}: {
  tone: keyof typeof NOTICE_TONE;
  title?: ReactNode;
  children?: ReactNode;
  testId?: string;
  role?: "alert" | "status";
  className?: string;
}) {
  return (
    <div
      role={role}
      data-testid={testId}
      className={`rounded-lg border px-3 py-2.5 text-[13px] leading-normal ${NOTICE_TONE[tone]} ${className}`}
    >
      {title && <p className="mb-1 font-semibold">{title}</p>}
      {children}
    </div>
  );
}

/** A panel's loading, empty or failed line. */
export function PanelMessage({
  children,
  tone = "dim",
  testId,
  role,
  className = "",
}: {
  children: ReactNode;
  tone?: "dim" | "error" | "warning";
  testId?: string;
  role?: "alert" | "status";
  className?: string;
}) {
  const color =
    tone === "error" ? "text-status-error-text" : tone === "warning" ? "text-status-warning" : "text-text-dim";
  return (
    <p role={role} data-testid={testId} className={`text-[13px] leading-normal ${color} ${className}`}>
      {children}
    </p>
  );
}

/** A full-width caution strip above the composer: what happened, an action, and a dismiss. */
export function WarningStrip({
  title,
  children,
  actions,
  onDismiss,
  dismissLabel = "Dismiss",
  testId,
  role,
}: {
  title: ReactNode;
  children?: ReactNode;
  actions?: ReactNode;
  onDismiss: () => void;
  dismissLabel?: string;
  testId?: string;
  role?: "status" | "alert";
}) {
  return (
    <div
      role={role}
      data-testid={testId}
      className="flex items-center gap-3 border-y border-status-warning/30 bg-status-warning/10 px-4 py-2 text-[13px] leading-snug text-status-warning"
    >
      <span className="min-w-0 flex-1 break-words">
        <span className="font-medium">{title}</span>
        {children && <> {children}</>}
      </span>
      {actions}
      <button
        type="button"
        onClick={onDismiss}
        aria-label={dismissLabel}
        className="flex h-7 w-7 max-md:h-11 max-md:w-11 shrink-0 cursor-pointer items-center justify-center rounded-md text-base text-status-warning/80 transition-colors hover:bg-status-warning/20 hover:text-status-warning"
      >
        &times;
      </button>
    </div>
  );
}

import { useLayoutEffect, type ReactNode } from "react";

/** Portal-free modal shell for the sidebar row pickers. Escape and backdrop clicks cancel unless `locked`. */
export function SidebarModal({
  testId,
  label,
  heading,
  title,
  subtitle,
  locked = false,
  onCancel,
  children,
}: {
  testId: string;
  label: string;
  heading: string;
  title: string;
  subtitle?: ReactNode;
  locked?: boolean;
  onCancel: () => void;
  children: ReactNode;
}) {
  // A layout effect so the listener sees the latest `locked` before a document keydown
  // can run; nothing flushes passive effects ahead of a native listener.
  useLayoutEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !locked) onCancel();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, locked]);

  return (
    <div
      data-testid={`${testId}-backdrop`}
      onClick={(e) => {
        if (e.target === e.currentTarget && !locked) onCancel();
      }}
      className="fixed inset-0 z-[60] flex items-center justify-center bg-black/60 px-4 py-8 overflow-y-auto"
      role="dialog"
      aria-modal="true"
      aria-label={label}
    >
      <div
        data-testid={testId}
        className="w-full max-w-sm rounded-lg border border-surface-700 bg-surface-800 shadow-xl"
      >
        <div className="px-4 py-3 border-b border-surface-700/40">
          <div className="text-sm font-mono text-text-primary truncate" title={title}>
            {heading}
            <span className="text-text-muted"> · {title}</span>
          </div>
          {subtitle}
        </div>
        {children}
      </div>
    </div>
  );
}

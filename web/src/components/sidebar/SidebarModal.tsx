import { useLayoutEffect, type ReactNode } from "react";
import { ModalFrame } from "../Dialog";

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
    <ModalFrame
      layer="z-[60]"
      rootProps={{ "data-testid": `${testId}-backdrop`, role: "dialog", "aria-modal": true, "aria-label": label }}
      panelProps={{ "data-testid": testId }}
      panelClassName="md:max-w-sm md:max-h-[90vh]"
      onBackdrop={locked ? undefined : onCancel}
    >
      <div className="px-4 pt-3 pb-2 max-md:pt-1">
        <div className="text-[15px] md:text-sm font-mono text-text-primary truncate" title={title}>
          {heading}
          <span className="text-text-muted"> · {title}</span>
        </div>
        {subtitle}
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto">{children}</div>
    </ModalFrame>
  );
}

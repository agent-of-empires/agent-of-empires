import { useRef, useState, type ReactNode } from "react";
import { ChevronDown } from "lucide-react";
import { useOutsideDismiss } from "./useOutsideDismiss";

/** A "label ▾" button that opens a popover; Esc closes it and returns focus to the button. */
export function SuggestionFilterMenu({
  label,
  active,
  children,
}: {
  label: string;
  active: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  useOutsideDismiss(open, [ref], () => setOpen(false));
  return (
    <div
      ref={ref}
      className="relative"
      onKeyDown={(e) => {
        if (e.key === "Escape" && open) {
          e.stopPropagation();
          setOpen(false);
          button.current?.focus();
        }
      }}
    >
      <button
        ref={button}
        type="button"
        aria-haspopup="true"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className={`inline-flex h-8 items-center gap-1.5 rounded-md border px-2.5 text-sm ${
          active
            ? "border-brand-500 bg-brand-500/10 text-text-primary"
            : "border-surface-700 text-text-secondary hover:bg-surface-800"
        }`}
      >
        {label}
        <ChevronDown className="h-3.5 w-3.5" />
      </button>
      {open && (
        <div className="absolute left-0 z-20 mt-1 max-h-72 min-w-[220px] overflow-y-auto rounded-md border border-surface-700 bg-surface-800 py-1 shadow-lg">
          {children}
        </div>
      )}
    </div>
  );
}

export function FilterMenuItem({
  selected,
  onClick,
  title,
  indent = false,
  children,
  count,
}: {
  selected: boolean;
  onClick: () => void;
  title?: string;
  indent?: boolean;
  children: ReactNode;
  count: number;
}) {
  return (
    <button
      type="button"
      aria-pressed={selected}
      title={title}
      onClick={onClick}
      className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs hover:bg-surface-700 ${indent ? "pl-6" : ""} ${
        selected ? "text-text-primary" : "text-text-secondary"
      }`}
    >
      <span aria-hidden="true" className="w-3">
        {selected ? "✓" : ""}
      </span>
      <span className="min-w-0 flex-1 truncate font-mono">{children}</span>
      <span className="text-text-dim">{count}</span>
    </button>
  );
}

import { useRef, useState } from "react";
import type { DiffComparison } from "../../lib/types";
import { useOutsideDismiss } from "../useOutsideDismiss";

const COMPARISONS: readonly { value: DiffComparison; label: string }[] = [
  { value: "base", label: "Base" },
  { value: "uncommitted", label: "Uncommitted" },
  { value: "upstream", label: "Tracking branch" },
];

interface Props {
  comparison: DiffComparison;
  onComparisonChange: (comparison: DiffComparison) => void;
  className?: string;
}

export function DiffComparisonSelect({ comparison, onComparisonChange, className }: Props) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const currentLabel = COMPARISONS.find(({ value }) => value === comparison)?.label ?? "Base";

  useOutsideDismiss(open, [rootRef], () => setOpen(false));

  return (
    <div ref={rootRef} role="group" aria-label="Diff comparison" className={`relative ${className ?? ""}`}>
      <button
        ref={triggerRef}
        type="button"
        aria-expanded={open}
        aria-label={`Diff comparison (current: ${currentLabel})`}
        onClick={() => setOpen((wasOpen) => !wasOpen)}
        className="inline-flex items-center gap-1 rounded border border-surface-700 bg-surface-800 px-1.5 py-1 font-mono text-[10px] text-text-primary cursor-pointer focus:border-brand-500 focus:outline-none"
      >
        {currentLabel}
        <span aria-hidden="true">▾</span>
      </button>
      {open && (
        <div
          className="absolute right-0 top-full z-50 mt-1 min-w-full bg-surface-800 border border-surface-700/50 rounded-md shadow-xl py-1"
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              event.preventDefault();
              setOpen(false);
              triggerRef.current?.focus();
            }
          }}
        >
          {COMPARISONS.map(({ value, label }) => {
            const selected = value === comparison;
            return (
              <button
                key={value}
                type="button"
                aria-pressed={selected}
                onClick={() => {
                  onComparisonChange(value);
                  setOpen(false);
                  triggerRef.current?.focus();
                }}
                className={`w-full whitespace-nowrap px-3 py-1.5 text-left font-mono text-[10px] cursor-pointer hover:bg-surface-700/60 ${
                  selected ? "text-brand-500" : "text-text-secondary hover:text-text-primary"
                }`}
              >
                {label}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

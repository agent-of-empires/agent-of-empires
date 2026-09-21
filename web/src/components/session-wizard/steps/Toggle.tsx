import type { ReactNode } from "react";

export function Toggle({
  checked,
  onChange,
  disabled,
  label,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  label?: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => !disabled && onChange(!checked)}
      className={`relative inline-flex h-7 w-12 shrink-0 items-center rounded-full transition-colors duration-200 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
        disabled ? "opacity-40 cursor-not-allowed" : "cursor-pointer"
      } ${checked ? "bg-brand-600" : "bg-surface-700"}`}
    >
      <span
        className={`inline-block h-5 w-5 rounded-full bg-white shadow-sm transition-transform duration-200 ${
          checked ? "translate-x-6" : "translate-x-1"
        }`}
      />
    </button>
  );
}

/** A full-width clickable card holding a title, description and switch. */
export function ToggleRow({
  title,
  description,
  checked,
  onChange,
  disabled,
  switchLabel,
  className = "cursor-pointer",
}: {
  title: string;
  description: ReactNode;
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  switchLabel?: string;
  className?: string;
}) {
  return (
    <label
      className={`flex items-center justify-between gap-3 p-3 bg-surface-900 border border-surface-700 rounded-lg ${className}`}
      onClick={(e) => {
        // The switch already fired onChange; handling the bubbled click would toggle twice.
        if (disabled || (e.target as HTMLElement).closest('button[role="switch"]')) return;
        onChange(!checked);
      }}
    >
      <div className="flex-1">
        <div className="text-sm font-medium text-text-primary">{title}</div>
        <div className="text-xs text-text-dim mt-0.5 leading-snug">{description}</div>
      </div>
      <Toggle checked={checked} onChange={onChange} disabled={disabled} label={switchLabel} />
    </label>
  );
}

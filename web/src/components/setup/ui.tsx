import { Check, Copy } from "lucide-react";
import { useState, type ReactNode } from "react";
import { writeClipboard } from "../../lib/clipboard";

export function Section({
  title,
  hint,
  action,
  children,
}: {
  title: string;
  hint?: ReactNode;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="flex flex-col gap-2">
      <div className="flex items-end justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold text-text-primary">{title}</h2>
          {hint && <p className="text-xs text-text-dim mt-0.5">{hint}</p>}
        </div>
        {action}
      </div>
      {children}
    </section>
  );
}

export const PANEL = "rounded-lg border border-surface-700 bg-surface-900 divide-y divide-surface-700";

export function RadioRow({
  name,
  checked,
  disabled,
  onSelect,
  children,
}: {
  name: string;
  checked: boolean;
  disabled?: boolean;
  onSelect: () => void;
  children: ReactNode;
}) {
  return (
    <label
      className={`flex items-center gap-3 px-4 min-h-11 py-2 cursor-pointer transition-colors ${checked ? "bg-surface-800" : "hover:bg-surface-800/50"} ${disabled ? "opacity-50 cursor-not-allowed" : ""}`}
    >
      <input
        type="radio"
        name={name}
        checked={checked}
        disabled={disabled}
        onChange={onSelect}
        className="accent-brand-600 h-4 w-4 shrink-0"
      />
      <div className="flex-1 min-w-0">{children}</div>
    </label>
  );
}

export function CopyCommand({ command }: { command: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-center gap-2 rounded-md border border-surface-700 bg-surface-950 px-3 h-9 font-mono text-xs text-text-primary">
      <span className="text-text-dim">$</span>
      <span className="flex-1 truncate" title={command}>
        {command}
      </span>
      <button
        type="button"
        aria-label={`Copy ${command}`}
        onClick={() => void writeClipboard(command).then(setCopied)}
        className="text-text-dim hover:text-text-primary cursor-pointer"
      >
        {copied ? <Check size={14} /> : <Copy size={14} />}
      </button>
    </div>
  );
}

export function GhostButton({
  onClick,
  disabled,
  children,
}: {
  onClick: () => void;
  disabled?: boolean;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className="h-8 px-3 rounded-md border border-surface-700 text-xs text-text-secondary hover:bg-surface-800 disabled:opacity-50 cursor-pointer"
    >
      {children}
    </button>
  );
}

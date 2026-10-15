import type { ReactNode } from "react";
import { Toggle } from "./steps/Toggle";

const ROW = "flex items-center gap-3 min-h-[44px] md:min-h-[36px] px-3 border-b border-surface-700/30 last:border-b-0";

function Chevron() {
  return (
    <svg className="w-3 h-3 shrink-0 text-text-dim" viewBox="0 0 12 12" aria-hidden="true">
      <path
        d="M4.5 2l4.5 4-4.5 4"
        stroke="currentColor"
        strokeWidth="1.5"
        fill="none"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function Label({ children }: { children: ReactNode }) {
  return <span className="w-24 shrink-0 text-sm text-text-secondary">{children}</span>;
}

/** Bordered group holding one-line wizard rows. */
export function RowGroup({ children }: { children: ReactNode }) {
  return <div className="bg-surface-900 border border-surface-700/60 rounded-lg">{children}</div>;
}

/** A row whose value is edited in a sub-panel, like a TUI Ctrl+P overlay. */
export function NavRow({
  label,
  value,
  placeholder,
  onOpen,
  testId,
  highlight = false,
}: {
  label: string;
  value: ReactNode;
  /** Shown dimmed when `value` is empty. */
  placeholder?: string;
  onOpen: () => void;
  testId?: string;
  /** Draws attention to a required, still-empty row. */
  highlight?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onOpen}
      data-testid={testId}
      className={`${ROW} w-full text-left cursor-pointer hover:bg-surface-850 transition-colors first:rounded-t-lg last:rounded-b-lg ${
        highlight ? "outline outline-1 outline-brand-600 -outline-offset-1" : ""
      }`}
    >
      <Label>{label}</Label>
      <span className="flex-1 min-w-0 truncate text-sm font-mono text-text-primary">
        {value || <span className="text-text-dim">{placeholder}</span>}
      </span>
      <Chevron />
    </button>
  );
}

/** A switch row; `onConfigure` adds a tappable summary that opens its sub-panel. */
export function SwitchRow({
  label,
  checked,
  onChange,
  switchLabel,
  summary,
  onConfigure,
  disabled = false,
  testId,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
  switchLabel: string;
  summary?: ReactNode;
  onConfigure?: () => void;
  disabled?: boolean;
  testId?: string;
}) {
  const summaryText = <span className="flex-1 min-w-0 truncate text-xs text-text-dim">{summary}</span>;
  return (
    <div className={ROW} data-testid={testId}>
      <Label>{label}</Label>
      {onConfigure ? (
        <button
          type="button"
          onClick={onConfigure}
          aria-label={`Configure ${label.toLowerCase()}`}
          className="flex-1 min-w-0 self-stretch flex items-center gap-2 text-left cursor-pointer hover:text-text-secondary"
        >
          {summaryText}
          <Chevron />
        </button>
      ) : (
        summaryText
      )}
      <Toggle checked={checked} onChange={onChange} disabled={disabled} label={switchLabel} />
    </div>
  );
}

/** A row holding an inline control, such as the title input. */
export function FieldRow({ label, htmlFor, children }: { label: string; htmlFor?: string; children: ReactNode }) {
  return (
    <div className={ROW}>
      <label htmlFor={htmlFor} className="w-24 shrink-0 text-sm text-text-secondary">
        {label}
      </label>
      <div className="flex-1 min-w-0">{children}</div>
    </div>
  );
}

// 16px text keeps iOS from zooming on focus; the placeholder matches the rows' 14px sans.
export const ROW_INPUT =
  "w-full min-w-0 bg-transparent py-2 text-base md:text-sm font-mono text-text-primary placeholder:font-sans placeholder:text-sm placeholder:text-text-dim focus:outline-none";

/** Header of a sub-panel that replaces the main form, with a Back action. */
export function PanelHeader({ title, onBack }: { title: string; onBack: () => void }) {
  return (
    <div className="flex items-center gap-2 mb-4">
      <button
        type="button"
        onClick={onBack}
        className="flex items-center gap-1 text-sm text-brand-500 hover:text-brand-400 cursor-pointer -ml-1 px-1 py-1"
      >
        <svg className="w-3 h-3 rotate-180" viewBox="0 0 12 12" aria-hidden="true">
          <path
            d="M4.5 2l4.5 4-4.5 4"
            stroke="currentColor"
            strokeWidth="1.5"
            fill="none"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
        Back
      </button>
      <h2 className="text-sm font-semibold text-text-primary">{title}</h2>
    </div>
  );
}

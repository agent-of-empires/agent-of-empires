import { useState, type ReactNode } from "react";

const CONTROL = "w-full bg-surface-900 border border-surface-700 rounded-md px-3 py-2 text-sm text-text-primary";
const FOCUS = "focus:border-brand-600 focus:outline-none";

/** Label above an optional description above the control. A `label` of `undefined` drops the element. */
function FieldShell({
  label,
  description,
  labelClassName,
  children,
}: {
  label?: string;
  description?: string;
  labelClassName?: string;
  children: ReactNode;
}) {
  return (
    <div>
      {label !== undefined && (
        <label className={labelClassName ?? "block text-sm text-text-bright mb-1"}>{label}</label>
      )}
      {description && <div className="text-xs text-text-dim mb-1">{description}</div>}
      {children}
    </div>
  );
}

/** Edits a local draft while focused and commits on blur, so typing never fights the saved value. */
function useDraft(value: string, onCommit: (draft: string) => void) {
  const [local, setLocal] = useState(value);
  const [focused, setFocused] = useState(false);

  if (!focused && local !== value) setLocal(value);

  const commit = () => {
    onCommit(local);
    setFocused(false);
  };
  return {
    commit,
    props: {
      value: local,
      onChange: (e: { target: { value: string } }) => setLocal(e.target.value),
      onFocus: () => setFocused(true),
      onBlur: commit,
    },
  };
}

export function CollapsibleSection({
  title,
  subtitle,
  badge,
  children,
  defaultOpen = false,
}: {
  title: string;
  subtitle?: string;
  badge?: string;
  children: React.ReactNode;
  defaultOpen?: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="border border-surface-700/40 rounded-lg overflow-hidden">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
        className="flex items-center justify-between w-full px-4 py-3 bg-surface-850 hover:bg-surface-800 cursor-pointer transition-colors text-left"
      >
        <div className="flex items-center gap-2">
          <svg
            className={`w-3 h-3 text-text-dim transition-transform ${open ? "rotate-90" : ""}`}
            viewBox="0 0 12 12"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M4.5 2l4.5 4-4.5 4" />
          </svg>
          <div>
            <span className="text-sm font-medium text-text-primary">{title}</span>
            {subtitle && <div className="text-[11px] text-text-dim mt-0.5">{subtitle}</div>}
          </div>
          {badge && (
            <span className="text-[10px] font-mono text-text-dim bg-surface-700 px-1.5 py-0.5 rounded">{badge}</span>
          )}
        </div>
      </button>
      {open && <div className="px-4 py-4 space-y-4 border-t border-surface-700/20">{children}</div>}
    </div>
  );
}

/** Checkbox row used by the per-browser preference panels. */
export function CheckboxRow({
  title,
  description,
  checked,
  onChange,
}: {
  title: string;
  description: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div>
      <label className="flex items-center justify-between gap-3 cursor-pointer">
        <div>
          <div className="text-[13px] text-text-secondary">{title}</div>
          <p className="text-[11px] text-text-muted mt-1">{description}</p>
        </div>
        <input
          type="checkbox"
          checked={checked}
          onChange={(e) => onChange(e.target.checked)}
          className="accent-brand-600 w-4 h-4 shrink-0"
        />
      </label>
    </div>
  );
}

export function ToggleField({
  label,
  description,
  checked,
  onChange,
}: {
  label: string;
  description?: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-center justify-between gap-3">
      <div>
        <div className="text-sm text-text-primary">{label}</div>
        {description && <div className="text-xs text-text-dim mt-0.5">{description}</div>}
      </div>
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        onClick={() => onChange(!checked)}
        className={`relative inline-flex h-6 w-10 shrink-0 items-center rounded-full transition-colors cursor-pointer ${checked ? "bg-brand-600" : "bg-surface-700"}`}
      >
        <span
          className={`inline-block h-4 w-4 rounded-full bg-white shadow-sm transition-transform ${checked ? "translate-x-5" : "translate-x-1"}`}
        />
      </button>
    </div>
  );
}

export function TextField({
  label,
  description,
  value,
  onChange,
  placeholder,
  mono,
  multiline,
}: {
  label: string;
  description?: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  mono?: boolean;
  multiline?: boolean;
}) {
  const draft = useDraft(value, (next) => {
    if (next !== value) onChange(next);
  });

  const cls = `${CONTROL} placeholder:text-text-dim ${FOCUS} ${mono ? "font-mono" : ""}`;
  return (
    <FieldShell label={label} description={description}>
      {multiline ? (
        <textarea
          {...draft.props}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              draft.commit();
            }
          }}
          placeholder={placeholder}
          rows={3}
          className={cls + " resize-y"}
        />
      ) : (
        <input
          type="text"
          {...draft.props}
          onKeyDown={(e) => {
            if (e.key === "Enter") draft.commit();
          }}
          placeholder={placeholder}
          className={cls}
        />
      )}
    </FieldShell>
  );
}

export function SelectField({
  label,
  description,
  value,
  onChange,
  options,
  labelClassName,
}: {
  label: string;
  description?: string;
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string }[];
  /** Overrides the label classes; `""` hides the label. */
  labelClassName?: string;
}) {
  return (
    <FieldShell label={label || undefined} description={description} labelClassName={labelClassName}>
      <select value={value} onChange={(e) => onChange(e.target.value)} className={`${CONTROL} ${FOCUS}`}>
        {options.map((opt) => (
          <option key={opt.value} value={opt.value}>
            {opt.label}
          </option>
        ))}
      </select>
    </FieldShell>
  );
}

export function NumberField({
  label,
  description,
  value,
  onChange,
  min,
  max,
}: {
  label: string;
  description?: string;
  value: number;
  onChange: (v: number) => void;
  min?: number;
  max?: number;
}) {
  const draft = useDraft(String(value), (next) => {
    const n = Number(next);
    if (!isNaN(n) && n !== value) onChange(n);
  });

  return (
    <FieldShell label={label} description={description}>
      <input
        type="number"
        {...draft.props}
        onKeyDown={(e) => {
          if (e.key === "Enter") draft.commit();
        }}
        min={min}
        max={max}
        className={`${CONTROL} ${FOCUS}`}
      />
    </FieldShell>
  );
}

export function SliderField({
  label,
  description,
  value,
  onChange,
  min,
  max,
  step,
  formatValue,
}: {
  label: string;
  description?: string;
  value: number;
  onChange: (v: number) => void;
  min: number;
  max: number;
  step: number;
  formatValue?: (v: number) => string;
}) {
  return (
    <div>
      <div className="flex items-center justify-between mb-1">
        <label className="text-sm text-text-bright">{label}</label>
        <span className="text-sm font-mono text-text-primary">{formatValue ? formatValue(value) : value}</span>
      </div>
      {description && <div className="text-xs text-text-dim mb-1">{description}</div>}
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        className="w-full accent-brand-600 h-1.5"
      />
    </div>
  );
}

export function ListField({
  label,
  description,
  items,
  onChange,
  placeholder,
  validate,
}: {
  label: string;
  description?: string;
  items: string[];
  onChange: (items: string[]) => void;
  placeholder?: string;
  validate?: (value: string) => string | null;
}) {
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);

  const cancel = () => {
    setAdding(false);
    setDraft("");
    setError(null);
  };

  const submit = () => {
    const trimmed = draft.trim();
    if (!trimmed) return;
    const err = validate?.(trimmed);
    if (err) {
      setError(err);
      return;
    }
    onChange([...items, trimmed]);
    cancel();
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-1">
        <label className="text-sm text-text-bright">{label}</label>
        {!adding && (
          <button
            onClick={() => setAdding(true)}
            className="text-xs text-brand-500 hover:text-brand-400 cursor-pointer"
          >
            + Add
          </button>
        )}
      </div>
      {description && <div className="text-xs text-text-dim mb-2">{description}</div>}
      {items.length === 0 && !adding && <div className="text-xs text-text-dim italic py-2">No items configured</div>}
      <div className="space-y-1 max-h-[320px] overflow-y-auto">
        {items.map((item, i) => (
          <div key={i} className="flex items-center justify-between gap-2 px-2 py-1.5 bg-surface-900 rounded group">
            <span className="text-sm font-mono text-text-primary truncate">{item}</span>
            <button
              onClick={() => onChange(items.filter((_, j) => j !== i))}
              className="text-text-dim hover:text-red-400 opacity-0 group-hover:opacity-100 transition-opacity cursor-pointer shrink-0"
              title="Remove"
            >
              <svg className="w-3.5 h-3.5" viewBox="0 0 16 16" fill="currentColor">
                <path d="M5.5 5.5A.5.5 0 0 1 6 6v6a.5.5 0 0 1-1 0V6a.5.5 0 0 1 .5-.5m2.5 0a.5.5 0 0 1 .5.5v6a.5.5 0 0 1-1 0V6a.5.5 0 0 1 .5-.5m3 .5a.5.5 0 0 0-1 0v6a.5.5 0 0 0 1 0z" />
                <path d="M14.5 3a1 1 0 0 1-1 1H13v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V4h-.5a1 1 0 0 1 0-2H6a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1h3.5a1 1 0 0 1 1 1M4.118 4 4 4.059V13a1 1 0 0 0 1 1h6a1 1 0 0 0 1-1V4.059L11.882 4z" />
              </svg>
            </button>
          </div>
        ))}
      </div>
      {adding && (
        <div className="mt-2">
          <div className="flex gap-2">
            <input
              type="text"
              value={draft}
              onChange={(e) => {
                setDraft(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") submit();
                if (e.key === "Escape") cancel();
              }}
              placeholder={placeholder}
              autoFocus
              className={`flex-1 bg-surface-900 border rounded-md px-3 py-1.5 text-sm font-mono text-text-primary placeholder:text-text-dim focus:outline-none ${error ? "border-red-500" : "border-surface-700 focus:border-brand-600"}`}
            />
            <button
              onClick={submit}
              className="px-3 py-1.5 rounded-md bg-brand-600 hover:bg-brand-500 text-sm font-medium text-surface-950 cursor-pointer"
            >
              Add
            </button>
            <button
              onClick={cancel}
              className="px-2 py-1.5 text-sm text-text-dim hover:text-text-primary cursor-pointer"
            >
              Cancel
            </button>
          </div>
          {error && <div className="text-xs text-red-400 mt-1">{error}</div>}
        </div>
      )}
    </div>
  );
}

import { useId } from "react";
import { INPUT } from "../../../lib/surfaces";
import { FIELD_HINT, FIELD_LABEL } from "../../../lib/surfaces";

const TEXT_INPUT = `${INPUT} font-mono`;

export function LabeledInput({
  label,
  value,
  onChange,
  placeholder,
  autoFocus = false,
  children,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder: string;
  autoFocus?: boolean;
  /** Hint rendered under the input. */
  children?: React.ReactNode;
}) {
  const id = useId();
  return (
    <div>
      <label htmlFor={id} className={FIELD_LABEL}>
        {label}
      </label>
      <input
        id={id}
        type="text"
        autoFocus={autoFocus}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className={TEXT_INPUT}
      />
      {children}
    </div>
  );
}

export function EnvVarList({
  values,
  onChange,
  label = "Environment variables",
  hint,
}: {
  values: string[];
  onChange: (v: string[]) => void;
  label?: string;
  hint?: string;
}) {
  return (
    <fieldset className="m-0 min-w-0 border-0 p-0">
      <legend className={FIELD_LABEL}>{label}</legend>
      {hint && <p className={`${FIELD_HINT} mb-1.5`}>{hint}</p>}
      {values.map((env, i) => (
        <div key={i} className="flex gap-2 mb-1">
          <input
            type="text"
            value={env}
            onChange={(e) => onChange(values.map((v, j) => (j === i ? e.target.value : v)))}
            placeholder="KEY=value"
            aria-label={`${label} ${i + 1}`}
            className={`${TEXT_INPUT} flex-1`}
          />
          <button
            aria-label={`Remove ${label} ${i + 1}`}
            onClick={() => onChange(values.filter((_, j) => j !== i))}
            type="button"
            className="shrink-0 w-9 max-md:w-11 rounded-lg text-text-dim hover:text-status-error hover:bg-surface-700/50 cursor-pointer"
          >
            &times;
          </button>
        </div>
      ))}
      <button
        aria-label={`Add ${label}`}
        onClick={() => onChange([...values, ""])}
        type="button"
        className="min-h-11 md:min-h-0 text-[13px] md:text-xs text-text-dim hover:text-text-secondary cursor-pointer"
      >
        + Add variable
      </button>
    </fieldset>
  );
}

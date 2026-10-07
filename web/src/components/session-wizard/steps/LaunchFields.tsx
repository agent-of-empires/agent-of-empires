import { useId } from "react";

const TEXT_INPUT =
  "w-full bg-surface-900 border border-surface-700 rounded-lg px-3 py-2.5 text-base md:text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none";

export function LabeledInput({
  label,
  value,
  onChange,
  placeholder,
  children,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder: string;
  /** Hint rendered under the input. */
  children?: React.ReactNode;
}) {
  const id = useId();
  return (
    <div>
      <label htmlFor={id} className="block text-sm text-text-dim mb-1.5">
        {label}
      </label>
      <input
        id={id}
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className={TEXT_INPUT}
      />
      {children}
    </div>
  );
}

export function EnvVarList({ values, onChange }: { values: string[]; onChange: (v: string[]) => void }) {
  return (
    <div>
      <label className="block text-sm text-text-dim mb-1.5">Environment variables</label>
      {values.map((env, i) => (
        <div key={i} className="flex gap-2 mb-1">
          <input
            type="text"
            value={env}
            onChange={(e) => onChange(values.map((v, j) => (j === i ? e.target.value : v)))}
            placeholder="KEY=value"
            className="flex-1 bg-surface-900 border border-surface-700 rounded-md px-2 py-1.5 text-base md:text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
          />
          <button
            onClick={() => onChange(values.filter((_, j) => j !== i))}
            className="px-2 text-text-dim hover:text-status-error cursor-pointer"
          >
            &times;
          </button>
        </div>
      ))}
      <button
        onClick={() => onChange([...values, ""])}
        className="text-xs text-text-dim hover:text-text-secondary cursor-pointer"
      >
        + Add variable
      </button>
    </div>
  );
}

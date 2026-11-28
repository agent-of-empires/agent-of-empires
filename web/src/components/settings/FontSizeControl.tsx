import { MAX_FONT_SIZE, MIN_FONT_SIZE } from "../../lib/fontSizeRange";
import { INPUT } from "../../lib/surfaces";
import { FIELD_DESCRIPTION, FIELD_LABEL } from "../../lib/surfaces";

interface FontSizeControlProps {
  label: string;
  value: number;
  onChange: (value: number) => void;
  description?: string;
  testIdPrefix: string;
}

const OPTIONS = Array.from({ length: MAX_FONT_SIZE - MIN_FONT_SIZE + 1 }, (_, i) => MIN_FONT_SIZE + i);

/** Synchronized slider and px select spanning MIN_FONT_SIZE to MAX_FONT_SIZE. */
export function FontSizeControl({ label, value, onChange, description, testIdPrefix }: FontSizeControlProps) {
  return (
    <div>
      <label className={description ? FIELD_LABEL : FIELD_LABEL.replace("mb-0.5", "mb-2")}>{label}</label>
      {description && <p className={FIELD_DESCRIPTION}>{description}</p>}
      <div className="flex items-center gap-3">
        <input
          type="range"
          aria-label={label}
          data-testid={`${testIdPrefix}-slider`}
          min={MIN_FONT_SIZE}
          max={MAX_FONT_SIZE}
          step={1}
          value={value}
          onChange={(e) => onChange(Number(e.target.value))}
          className="flex-1 accent-brand-600 h-1.5"
        />
        <select
          aria-label={`${label} (px)`}
          data-testid={`${testIdPrefix}-select`}
          value={value}
          onChange={(e) => onChange(Number(e.target.value))}
          className={`${INPUT} !w-20 !px-2 font-mono text-center`}
        >
          {OPTIONS.map((s) => (
            <option key={s} value={s}>
              {s}px
            </option>
          ))}
        </select>
      </div>
    </div>
  );
}

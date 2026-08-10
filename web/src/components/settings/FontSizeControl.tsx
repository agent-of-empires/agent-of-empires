import { MAX_FONT_SIZE, MIN_FONT_SIZE } from "../../lib/fontSizeRange";

interface FontSizeControlProps {
  label: string;
  value: number;
  onChange: (value: number) => void;
  description?: string;
  /** Prefix for the slider/select `data-testid`s, so a settings panel with two
   *  of these controls stays addressable in tests. */
  testIdPrefix: string;
}

const OPTIONS = Array.from({ length: MAX_FONT_SIZE - MIN_FONT_SIZE + 1 }, (_, i) => MIN_FONT_SIZE + i);

/** Slider + px select pair shared by the terminal and conversation font-size
 *  settings. Both controls write the same value so they stay synchronized; the
 *  select offers every integer step in range. The range is not a prop: every
 *  font-size control in the dashboard spans {@link MIN_FONT_SIZE}-{@link
 *  MAX_FONT_SIZE}, and users compare the sliders side by side. */
export function FontSizeControl({ label, value, onChange, description, testIdPrefix }: FontSizeControlProps) {
  return (
    <div>
      <label className="block text-[13px] text-text-secondary mb-2">{label}</label>
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
          className="bg-surface-800 border border-surface-700 rounded-md px-2 py-1 text-sm text-text-primary font-mono w-16 text-center"
        >
          {OPTIONS.map((s) => (
            <option key={s} value={s}>
              {s}px
            </option>
          ))}
        </select>
      </div>
      {description && <p className="text-[11px] text-text-muted mt-1">{description}</p>}
    </div>
  );
}

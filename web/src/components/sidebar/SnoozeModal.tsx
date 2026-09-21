import { useState } from "react";
import { SNOOZE_MAX_MINUTES, SNOOZE_PRESETS } from "./format";
import { SidebarModal } from "./SidebarModal";

const UNITS = {
  m: { label: "minutes", minutes: 1 },
  h: { label: "hours", minutes: 60 },
  d: { label: "days", minutes: 1440 },
  w: { label: "weeks", minutes: 10080 },
};
type SnoozeUnit = keyof typeof UNITS;

const FIELD =
  "rounded border border-surface-700 bg-surface-900 px-2 py-1 text-sm text-text-primary focus:border-brand-600 focus:outline-none";
const SUBMIT =
  "rounded bg-brand-600 px-3 py-1 text-sm font-medium text-text-primary hover:bg-brand-500 cursor-pointer transition-colors";

function customMinutes(value: string, unit: SnoozeUnit): number | string {
  const n = Number.parseInt(value, 10);
  if (!Number.isFinite(n) || n <= 0) return "Enter a positive whole number.";
  const minutes = n * UNITS[unit].minutes;
  if (minutes < 1 || minutes > SNOOZE_MAX_MINUTES) {
    return `Must be between 1 minute and 30 days (got ${minutes} minutes).`;
  }
  return minutes;
}

function untilMinutes(value: string): number | string {
  if (!value) return "Pick a date and time.";
  // datetime-local has no zone, so Date.parse reads it as the user's local time.
  const target = Date.parse(value);
  if (!Number.isFinite(target)) return "Invalid date.";
  const deltaMs = target - Date.now();
  if (deltaMs <= 0) return "Pick a time in the future.";
  const minutes = Math.max(1, Math.round(deltaMs / 60_000));
  return minutes > SNOOZE_MAX_MINUTES ? "Maximum snooze is 30 days from now." : minutes;
}

/** Snooze picker: TUI presets, a custom number plus unit, or an absolute date and time. */
export function SnoozeModal({
  title,
  onCancel,
  onPick,
}: {
  title: string;
  onCancel: () => void;
  onPick: (minutes: number) => void;
}) {
  const [customValue, setCustomValue] = useState("");
  const [customUnit, setCustomUnit] = useState<SnoozeUnit>("h");
  const [untilValue, setUntilValue] = useState("");
  const [customError, setCustomError] = useState<string | null>(null);
  const [untilError, setUntilError] = useState<string | null>(null);

  const submit = (result: number | string, setError: (e: string | null) => void) => {
    if (typeof result === "string") return setError(result);
    setError(null);
    onPick(result);
  };
  const submitCustom = () => submit(customMinutes(customValue, customUnit), setCustomError);
  const submitUntil = () => submit(untilMinutes(untilValue), setUntilError);

  return (
    <SidebarModal
      testId="snooze-modal"
      label="Snooze session"
      heading="Snooze"
      title={title}
      subtitle={<div className="mt-1 text-[11px] text-text-dim">How long should this session sit out?</div>}
      onCancel={onCancel}
    >
      <div className="flex flex-col py-2">
        {SNOOZE_PRESETS.map((preset) => (
          <button
            key={preset.minutes}
            onClick={() => onPick(preset.minutes)}
            data-testid={`snooze-modal-preset-${preset.minutes}`}
            className="w-full text-left px-4 py-2 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
          >
            {preset.label}
          </button>
        ))}
      </div>
      <SnoozeSection label="Custom duration" error={customError} errorTestId="snooze-modal-custom-error">
        <input
          type="number"
          inputMode="numeric"
          min={1}
          value={customValue}
          onChange={(e) => setCustomValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") submitCustom();
          }}
          placeholder="3"
          data-testid="snooze-modal-custom-value"
          aria-label="Custom snooze duration"
          className={`w-20 ${FIELD}`}
        />
        <select
          value={customUnit}
          onChange={(e) => setCustomUnit(e.target.value as SnoozeUnit)}
          data-testid="snooze-modal-custom-unit"
          aria-label="Custom snooze unit"
          className={FIELD}
        >
          {(Object.keys(UNITS) as SnoozeUnit[]).map((u) => (
            <option key={u} value={u}>
              {UNITS[u].label}
            </option>
          ))}
        </select>
        <button onClick={submitCustom} data-testid="snooze-modal-custom-submit" className={`ml-auto ${SUBMIT}`}>
          Snooze
        </button>
      </SnoozeSection>
      <SnoozeSection label="Until" error={untilError} errorTestId="snooze-modal-until-error">
        <input
          type="datetime-local"
          value={untilValue}
          onChange={(e) => setUntilValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") submitUntil();
          }}
          data-testid="snooze-modal-until-value"
          aria-label="Snooze until"
          className={`flex-1 min-w-0 ${FIELD}`}
        />
        <button onClick={submitUntil} data-testid="snooze-modal-until-submit" className={SUBMIT}>
          Snooze
        </button>
      </SnoozeSection>
      <div className="px-4 py-3 border-t border-surface-700/40 flex justify-end">
        <button
          onClick={onCancel}
          data-testid="snooze-modal-cancel"
          className="text-sm text-text-dim hover:text-text-primary cursor-pointer transition-colors"
        >
          Cancel
        </button>
      </div>
    </SidebarModal>
  );
}

function SnoozeSection({
  label,
  error,
  errorTestId,
  children,
}: {
  label: string;
  error: string | null;
  errorTestId: string;
  children: React.ReactNode;
}) {
  return (
    <div className="px-4 py-3 border-t border-surface-700/40">
      <div className="text-[11px] font-mono uppercase tracking-widest text-text-muted mb-2">{label}</div>
      <div className="flex items-center gap-2">{children}</div>
      {error && (
        <div role="alert" data-testid={errorTestId} className="mt-1 text-[11px] text-status-error">
          {error}
        </div>
      )}
    </div>
  );
}

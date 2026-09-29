import type { ReactNode } from "react";

import { useWebSettings } from "../../hooks/useWebSettings";
import {
  DEFAULT_TOOLBAR_KEYS,
  MAX_TOOLBAR_KEYS,
  TOOLBAR_KEY_CATALOG,
  toolbarKeySpec,
  type ToolbarKeyId,
} from "../../lib/terminalToolbarKeys";

function RowButton({
  label,
  disabled,
  onClick,
  children,
}: {
  label: string;
  disabled?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
      className="w-8 h-8 shrink-0 rounded-md font-mono text-sm text-text-secondary hover:bg-surface-700 cursor-pointer disabled:opacity-40 disabled:cursor-default transition-colors"
    >
      {children}
    </button>
  );
}

/** Editor for the mobile live terminal's key row and the arrow joystick toggle. */
export function MobileKeysSettings() {
  const { settings, update } = useWebSettings();
  const keys = settings.mobileToolbarKeys;
  const setKeys = (next: ToolbarKeyId[]) => update({ mobileToolbarKeys: next });
  const move = (from: number, to: number) => {
    const next = [...keys];
    const [id] = next.splice(from, 1);
    next.splice(to, 0, id!);
    setKeys(next);
  };
  const available = TOOLBAR_KEY_CATALOG.filter((spec) => !keys.includes(spec.id));
  const full = keys.length >= MAX_TOOLBAR_KEYS;

  return (
    <div className="space-y-3">
      <div>
        <div className="flex items-center justify-between gap-3 mb-2">
          <div className="text-[13px] text-text-secondary">Mobile key row</div>
          <button
            type="button"
            onClick={() => setKeys([...DEFAULT_TOOLBAR_KEYS])}
            className="text-[12px] text-brand-500 hover:text-brand-400 cursor-pointer"
          >
            Reset to default
          </button>
        </div>
        <ol aria-label="Keys in the row" className="space-y-1">
          {keys.map((id, i) => {
            const spec = toolbarKeySpec(id);
            return (
              <li key={id} className="flex items-center gap-2 pl-2 rounded-md bg-surface-800 border border-surface-700">
                <span className="w-14 shrink-0 font-mono text-sm text-text-primary">{spec.label}</span>
                <span className="flex-1 min-w-0 truncate text-[12px] text-text-muted">{spec.name}</span>
                <RowButton label={`Move ${spec.name} up`} disabled={i === 0} onClick={() => move(i, i - 1)}>
                  {"↑"}
                </RowButton>
                <RowButton
                  label={`Move ${spec.name} down`}
                  disabled={i === keys.length - 1}
                  onClick={() => move(i, i + 1)}
                >
                  {"↓"}
                </RowButton>
                <RowButton label={`Remove ${spec.name}`} onClick={() => setKeys(keys.filter((k) => k !== id))}>
                  {"×"}
                </RowButton>
              </li>
            );
          })}
        </ol>
        {available.length > 0 && (
          <div className="flex flex-wrap gap-1 mt-2">
            {available.map((spec) => (
              <button
                key={spec.id}
                type="button"
                aria-label={`Add ${spec.name}`}
                disabled={full}
                onClick={() => setKeys([...keys, spec.id])}
                className="h-8 px-2 rounded-md border border-surface-700 font-mono text-[12px] text-text-secondary hover:bg-surface-800 cursor-pointer disabled:opacity-40 disabled:cursor-default transition-colors"
              >
                + {spec.label}
              </button>
            ))}
          </div>
        )}
        <p className="text-[11px] text-text-muted mt-1">
          Keys above the soft keyboard in the live terminal, left to right. Up to {MAX_TOOLBAR_KEYS} fit on one row.
        </p>
      </div>

      <label className="flex items-center justify-between gap-3 cursor-pointer">
        <div>
          <div className="text-[13px] text-text-secondary">Arrow joystick on mobile</div>
          <p className="text-[11px] text-text-muted mt-1">
            Drag the pad above the keyboard button to send arrow keys; drag further to repeat faster.
          </p>
        </div>
        <input
          type="checkbox"
          checked={settings.showArrowJoystick}
          onChange={(e) => update({ showArrowJoystick: e.target.checked })}
          className="accent-brand-600 w-4 h-4 shrink-0"
        />
      </label>
    </div>
  );
}

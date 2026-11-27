import type { ReactNode } from "react";
import type { ProfileInfo } from "../../../lib/types";
import { CHOICE, CHOICE_SELECTED, FIELD_HINT, FIELD_LABEL } from "../../../lib/surfaces";

function PresetOption({
  selected,
  onSelect,
  children,
}: {
  selected: boolean;
  onSelect: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      onClick={onSelect}
      className={`${selected ? CHOICE_SELECTED : CHOICE} cursor-pointer`}
    >
      {children}
    </button>
  );
}

/** Radio list of profiles, with the server's own active profile as the empty choice. */
export function ProfilePresetPicker({
  profiles,
  selected,
  dirty,
  onSelect,
}: {
  profiles: ProfileInfo[];
  selected: string;
  dirty: boolean;
  onSelect: (name: string) => void;
}) {
  return (
    <div className="mb-5">
      <label className={FIELD_LABEL}>Workflow preset</label>
      <p className={`${FIELD_HINT} mb-2`}>
        Profiles preload tool, sandbox, auto-approve, and env defaults for common workflows.
      </p>
      <div role="radiogroup" aria-label="Workflow preset" className="space-y-1.5">
        <PresetOption selected={selected === ""} onSelect={() => onSelect("")}>
          <div className="text-[15px] md:text-sm font-semibold text-text-primary">Server default</div>
          <div className={`mt-0.5 ${FIELD_HINT}`}>Use the active profile on the server with no client-side preset.</div>
        </PresetOption>
        {profiles.map((p) => (
          <PresetOption key={p.name} selected={selected === p.name} onSelect={() => onSelect(p.name)}>
            <div className="flex flex-wrap items-baseline gap-2">
              <span className="text-[15px] md:text-sm font-semibold text-text-primary">{p.name}</span>
              {p.is_default && (
                <span className="rounded px-1.5 py-px text-[10px] font-mono uppercase tracking-wide bg-surface-700 text-text-dim">
                  Active
                </span>
              )}
            </div>
            {p.description && <div className={`mt-0.5 ${FIELD_HINT}`}>{p.description}</div>}
          </PresetOption>
        ))}
      </div>
      {selected && dirty && (
        <p className="text-[13px] md:text-xs text-brand-500 mt-1">(Custom) Settings differ from preset defaults</p>
      )}
    </div>
  );
}

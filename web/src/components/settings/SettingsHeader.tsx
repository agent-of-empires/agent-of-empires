import { ProfileSelector } from "./ProfileSelector";

interface Props {
  onClose: () => void;
  saving: boolean;
  saveError: string | null;
  selectedProfile: string;
  onSelectProfile: (profile: string) => void;
}

// Settings header. ProfileSelector wraps onto its own row on mobile via
// `basis-full` so the Back affordance and title keep their space; on md+
// the wrapper switches to `basis-auto ml-auto` for a single-row layout
// with the picker aligned right.
export function SettingsHeader({
  onClose,
  saving,
  saveError,
  selectedProfile,
  onSelectProfile,
}: Props) {
  return (
    <div
      data-testid="settings-header"
      className="bg-surface-850 border-b border-surface-700 shrink-0 flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2 md:flex-nowrap md:h-12 md:py-0"
    >
      <button
        onClick={onClose}
        className="text-brand-500 cursor-pointer text-sm shrink-0"
      >
        &larr; Back
      </button>
      <span className="text-xs font-mono text-text-bright shrink-0">
        Settings
      </span>
      {saving && (
        <span className="text-[11px] font-mono text-text-dim shrink-0">
          Saving...
        </span>
      )}
      {saveError && (
        <span
          data-testid="settings-header-save-error"
          className="text-[11px] font-mono text-status-error truncate min-w-0"
        >
          {saveError}
        </span>
      )}
      <div className="basis-full flex justify-center overflow-x-auto md:basis-auto md:ml-auto md:overflow-visible md:justify-end shrink-0">
        <ProfileSelector
          selectedProfile={selectedProfile}
          onSelect={onSelectProfile}
        />
      </div>
    </div>
  );
}

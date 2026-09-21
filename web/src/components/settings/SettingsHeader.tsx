import { ProfileSelector } from "./ProfileSelector";
import { SettingsSearch } from "./SettingsSearch";
import type { SettingsFieldDescriptor } from "../../lib/types";
import type { SettingsSearchHit } from "./settingsSearchIndex";

interface Props {
  onClose: () => void;
  saving: boolean;
  saveError: string | null;
  selectedProfile: string;
  onSelectProfile: (profile: string) => void;
  schema: SettingsFieldDescriptor[];
  schemaLoading: boolean;
  onSearchJump: (hit: SettingsSearchHit) => void;
  hideProfileSelector?: boolean;
}

// On mobile the search and profile picker wrap onto their own rows.
export function SettingsHeader({
  onClose,
  saving,
  saveError,
  selectedProfile,
  onSelectProfile,
  schema,
  schemaLoading,
  onSearchJump,
  hideProfileSelector = false,
}: Props) {
  return (
    <div
      data-testid="settings-header"
      className="bg-surface-850 border-b border-surface-700 shrink-0 flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2 md:flex-nowrap md:h-12 md:py-0"
    >
      <button onClick={onClose} className="text-brand-500 cursor-pointer text-sm shrink-0">
        &larr; Back
      </button>
      <span className="text-xs font-mono text-text-bright shrink-0">Settings</span>
      {saving && <span className="text-[11px] font-mono text-text-dim shrink-0">Saving...</span>}
      {saveError && (
        <span
          data-testid="settings-header-save-error"
          className="text-[11px] font-mono text-status-error truncate min-w-0"
        >
          {saveError}
        </span>
      )}
      <div className="basis-full md:basis-auto md:flex-1 md:min-w-0 md:max-w-sm md:ml-auto">
        <SettingsSearch schema={schema} loading={schemaLoading} onJump={onSearchJump} />
      </div>
      {!hideProfileSelector && (
        <div className="basis-full flex justify-center overflow-x-auto md:basis-auto md:overflow-visible md:justify-end shrink-0">
          <ProfileSelector selectedProfile={selectedProfile} onSelect={onSelectProfile} />
        </div>
      )}
    </div>
  );
}

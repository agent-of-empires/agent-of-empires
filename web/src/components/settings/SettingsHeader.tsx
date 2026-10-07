import { SettingsSearch } from "./SettingsSearch";
import type { SettingsFieldDescriptor } from "../../lib/types";
import type { SettingsSearchHit } from "./settingsSearchIndex";

interface Props {
  onClose: () => void;
  saving: boolean;
  saveError: string | null;
  schema: SettingsFieldDescriptor[];
  schemaLoading: boolean;
  onSearchJump: (hit: SettingsSearchHit) => void;
  /** Set on a tab page: mobile Back returns to the section list instead of closing. */
  onBackToList?: () => void;
}

// On mobile the search wraps onto its own row.
export function SettingsHeader({
  onClose,
  saving,
  saveError,
  schema,
  schemaLoading,
  onSearchJump,
  onBackToList,
}: Props) {
  return (
    <div
      data-testid="settings-header"
      className="bg-surface-850 border-b border-surface-700 shrink-0 flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2 md:flex-nowrap md:h-12 md:py-0"
    >
      {onBackToList && (
        <button onClick={onBackToList} className="md:hidden text-brand-500 cursor-pointer text-sm shrink-0">
          &larr; All settings
        </button>
      )}
      <button
        onClick={onClose}
        className={`text-brand-500 cursor-pointer text-sm shrink-0 ${onBackToList ? "hidden md:inline" : ""}`}
      >
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
    </div>
  );
}

import { ChevronLeft } from "lucide-react";
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
  /** Set on a tab page: a phone's back returns to the section list instead of closing. */
  onBackToList?: () => void;
  /** The open section's name, which a phone's header titles its page with. */
  sectionLabel: string;
}

function SaveState({ saving, saveError }: Pick<Props, "saving" | "saveError">) {
  return (
    <>
      {saving && <span className="text-[12px] text-text-dim shrink-0">Saving…</span>}
      {saveError && (
        <span data-testid="settings-header-save-error" className="text-[12px] text-status-error truncate min-w-0">
          {saveError}
        </span>
      )}
    </>
  );
}

/**
 * A phone heads Settings as it heads the session list: a round back button beside a large title, and on the
 * section list a search pill under it. A wide screen keeps one quiet row: back, the page's name, and search.
 */
export function SettingsHeader({
  onClose,
  saving,
  saveError,
  schema,
  schemaLoading,
  onSearchJump,
  onBackToList,
  sectionLabel,
}: Props) {
  return (
    <div
      data-testid="settings-header"
      className="shrink-0 bg-surface-900 md:bg-transparent flex flex-wrap items-center gap-x-3 gap-y-3 px-4 pt-3 pb-3 md:flex-nowrap md:gap-x-2 md:h-12 md:px-3 md:py-0"
    >
      <button
        type="button"
        onClick={onBackToList ?? onClose}
        aria-label={onBackToList ? "All settings" : "Back"}
        className="md:hidden w-10 h-10 shrink-0 flex items-center justify-center rounded-full float-control text-text-secondary cursor-pointer active:scale-95 transition-transform"
      >
        <ChevronLeft className="size-5" strokeWidth={1.75} aria-hidden />
      </button>
      <h1 className="md:hidden min-w-0 truncate text-[22px] font-semibold text-text-primary">
        {onBackToList ? sectionLabel : "Settings"}
      </h1>
      <button
        type="button"
        onClick={onClose}
        className="hidden md:flex h-8 pl-1.5 pr-2.5 items-center gap-0.5 rounded-md text-[13px] text-text-secondary hover:text-text-primary hover:bg-surface-800/60 cursor-pointer transition-colors"
      >
        <ChevronLeft className="size-4" strokeWidth={1.75} aria-hidden />
        Back
      </button>
      <span className="hidden md:inline text-[13px] font-medium text-text-primary shrink-0">Settings</span>
      <SaveState saving={saving} saveError={saveError} />
      {/* On a phone the list's search takes its own row under the title; a section page drops it. */}
      <div
        className={`basis-full md:basis-auto md:flex-1 md:min-w-0 md:max-w-sm md:ml-auto ${onBackToList ? "max-md:hidden" : ""}`}
      >
        <SettingsSearch schema={schema} loading={schemaLoading} onJump={onSearchJump} />
      </div>
    </div>
  );
}

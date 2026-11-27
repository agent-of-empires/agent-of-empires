import { ChevronLeft } from "lucide-react";
import { createPortal } from "react-dom";
import { useHeaderPageSlot } from "../../lib/headerSlot";
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
 * section list a search pill under it. A wide screen has no row of its own: the app header shows Back and the
 * title, and this puts the save state and search in its middle.
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
  const slot = useHeaderPageSlot();
  if (slot) {
    return createPortal(
      <>
        <SaveState saving={saving} saveError={saveError} />
        <div className="flex-1 min-w-0 max-w-sm">
          <SettingsSearch schema={schema} loading={schemaLoading} onJump={onSearchJump} />
        </div>
      </>,
      slot,
    );
  }
  return (
    <div
      data-testid="settings-header"
      className="shrink-0 bg-surface-900 flex flex-wrap items-center gap-x-3 gap-y-3 px-4 pt-3 pb-3"
    >
      <button
        type="button"
        onClick={onBackToList ?? onClose}
        aria-label={onBackToList ? "All settings" : "Back"}
        className="w-10 h-10 shrink-0 flex items-center justify-center rounded-full float-control text-text-secondary cursor-pointer active:scale-95 transition-transform"
      >
        <ChevronLeft className="size-5" strokeWidth={1.75} aria-hidden />
      </button>
      <h1 className="min-w-0 truncate text-[22px] font-semibold text-text-primary">
        {onBackToList ? sectionLabel : "Settings"}
      </h1>
      <SaveState saving={saving} saveError={saveError} />
      {/* The list's search takes its own row under the title; a section page drops it. */}
      <div className={`basis-full ${onBackToList ? "hidden" : ""}`}>
        <SettingsSearch schema={schema} loading={schemaLoading} onJump={onSearchJump} />
      </div>
    </div>
  );
}

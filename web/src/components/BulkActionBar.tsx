import { useState } from "react";
import { Archive, Moon, Pin, X } from "lucide-react";

import type { BulkTriageBuckets } from "../lib/sidebarBulk";
import type { Workspace } from "../lib/types";

interface BulkActionBarProps {
  selectedCount: number;
  buckets: BulkTriageBuckets;
  snoozePresets: readonly { label: string; minutes: number }[];
  onBulkPin: (workspaces: Workspace[], pinned: boolean) => void;
  onBulkArchive: (workspaces: Workspace[], archived: boolean) => void;
  onBulkSnooze: (workspaces: Workspace[], minutes: number | null) => void;
  onClear: () => void;
}

const BTN =
  "inline-flex h-8 items-center gap-1 rounded-md border border-surface-700/50 bg-surface-800/60 px-2 text-[11px] font-mono text-text-secondary hover:bg-surface-700/60 hover:text-text-primary cursor-pointer transition-colors disabled:opacity-40 disabled:cursor-default";

/** Bulk triage bar shown while one or more sidebar rows are multi-selected.
 *  Actions split by eligibility so a mixed selection shows count-labelled
 *  buttons ("Pin 3" / "Unpin 2") instead of one ambiguous toggle; an action
 *  applies only to its compatible subset. See #1724. */
export function BulkActionBar({
  selectedCount,
  buckets,
  snoozePresets,
  onBulkPin,
  onBulkArchive,
  onBulkSnooze,
  onClear,
}: BulkActionBarProps) {
  const [snoozeOpen, setSnoozeOpen] = useState(false);
  if (selectedCount === 0) return null;
  return (
    <div
      data-testid="sidebar-bulk-bar"
      className="flex flex-wrap items-center gap-2 border-b border-surface-700/40 bg-surface-850 px-3 py-2"
    >
      <span className="mr-1 text-[11px] font-mono uppercase tracking-widest text-text-muted">
        {selectedCount} selected
      </span>
      {buckets.pinnable.length > 0 && (
        <button
          type="button"
          data-testid="sidebar-bulk-pin"
          className={BTN}
          onClick={() => onBulkPin(buckets.pinnable, true)}
        >
          <Pin className="h-3 w-3 -rotate-45" />
          Pin {buckets.pinnable.length}
        </button>
      )}
      {buckets.unpinnable.length > 0 && (
        <button
          type="button"
          data-testid="sidebar-bulk-unpin"
          className={BTN}
          onClick={() => onBulkPin(buckets.unpinnable, false)}
        >
          <Pin className="h-3 w-3 -rotate-45" />
          Unpin {buckets.unpinnable.length}
        </button>
      )}
      {buckets.archivable.length > 0 && (
        <button
          type="button"
          data-testid="sidebar-bulk-archive"
          className={BTN}
          onClick={() => onBulkArchive(buckets.archivable, true)}
        >
          <Archive className="h-3 w-3" />
          Archive {buckets.archivable.length}
        </button>
      )}
      {buckets.unarchivable.length > 0 && (
        <button
          type="button"
          data-testid="sidebar-bulk-unarchive"
          className={BTN}
          onClick={() => onBulkArchive(buckets.unarchivable, false)}
        >
          <Archive className="h-3 w-3" />
          Unarchive {buckets.unarchivable.length}
        </button>
      )}
      {buckets.snoozable.length > 0 && (
        <div className="relative">
          <button
            type="button"
            data-testid="sidebar-bulk-snooze"
            className={BTN}
            aria-haspopup="menu"
            aria-expanded={snoozeOpen}
            onClick={() => setSnoozeOpen((o) => !o)}
          >
            <Moon className="h-3 w-3" />
            Snooze {buckets.snoozable.length}
          </button>
          {snoozeOpen && (
            <div
              data-testid="sidebar-bulk-snooze-menu"
              role="menu"
              className="absolute left-0 z-50 mt-1 min-w-28 rounded border border-surface-700/60 bg-surface-900 py-1 shadow-lg"
            >
              {snoozePresets.map((preset) => (
                <button
                  type="button"
                  key={preset.minutes}
                  role="menuitem"
                  className="block w-full px-3 py-1.5 text-left text-[12px] font-mono text-text-secondary hover:bg-surface-700/50 hover:text-text-primary cursor-pointer"
                  onClick={() => {
                    setSnoozeOpen(false);
                    onBulkSnooze(buckets.snoozable, preset.minutes);
                  }}
                >
                  {preset.label}
                </button>
              ))}
            </div>
          )}
        </div>
      )}
      {buckets.unsnoozable.length > 0 && (
        <button
          type="button"
          data-testid="sidebar-bulk-unsnooze"
          className={BTN}
          onClick={() => onBulkSnooze(buckets.unsnoozable, null)}
        >
          <Moon className="h-3 w-3" />
          Unsnooze {buckets.unsnoozable.length}
        </button>
      )}
      <button
        type="button"
        data-testid="sidebar-bulk-clear"
        className={`${BTN} ml-auto`}
        onClick={onClear}
        aria-label="Clear selection"
      >
        <X className="h-3 w-3" />
        Clear
      </button>
    </div>
  );
}

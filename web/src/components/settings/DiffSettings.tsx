import { useWebSettings } from "../../hooks/useWebSettings";

/// Client-side diff view preferences. These are pure rendering choices stored
/// per browser in `localStorage` (like the Terminal section), not backend
/// config, so they need no server round-trip or elevation. The same toggles
/// are also reachable inline from the diff view itself.
export function DiffSettings() {
  const { settings, update } = useWebSettings();

  return (
    <div>
      <div className="space-y-4">
        <div>
          <label className="flex items-center justify-between gap-3 cursor-pointer">
            <div>
              <div className="text-[13px] text-text-secondary">
                Side-by-side diff
              </div>
              <p className="text-[11px] text-text-muted mt-1">
                Show diffs in a split (side-by-side) layout instead of unified.
                On narrow screens the diff falls back to unified automatically.
              </p>
            </div>
            <input
              type="checkbox"
              checked={settings.diffViewLayout === "split"}
              onChange={(e) =>
                update({ diffViewLayout: e.target.checked ? "split" : "unified" })
              }
              className="accent-brand-600 w-4 h-4 shrink-0"
            />
          </label>
        </div>

        <div>
          <label className="flex items-center justify-between gap-3 cursor-pointer">
            <div>
              <div className="text-[13px] text-text-secondary">
                Tree file list
              </div>
              <p className="text-[11px] text-text-muted mt-1">
                Group changed files into a collapsible directory tree. Turn off
                for a flat list of file paths.
              </p>
            </div>
            <input
              type="checkbox"
              checked={settings.diffViewMode === "tree"}
              onChange={(e) =>
                update({ diffViewMode: e.target.checked ? "tree" : "flat" })
              }
              className="accent-brand-600 w-4 h-4 shrink-0"
            />
          </label>
        </div>
      </div>
    </div>
  );
}

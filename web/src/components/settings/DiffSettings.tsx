import { useWebSettings } from "../../hooks/useWebSettings";
import { CheckboxRow } from "./FormFields";

/// Per-browser diff preferences, also reachable from the diff view.
export function DiffSettings() {
  const { settings, update } = useWebSettings();

  return (
    <div>
      <div className="space-y-4">
        <CheckboxRow
          title="Side-by-side diff"
          description="Show diffs in a split (side-by-side) layout instead of unified. On narrow screens the diff falls back to unified automatically."
          checked={settings.diffViewLayout === "split"}
          onChange={(v) => update({ diffViewLayout: v ? "split" : "unified" })}
        />
        <CheckboxRow
          title="Tree file list"
          description="Group changed files into a collapsible directory tree. Turn off for a flat list of file paths."
          checked={settings.diffViewMode === "tree"}
          onChange={(v) => update({ diffViewMode: v ? "tree" : "flat" })}
        />
      </div>
    </div>
  );
}

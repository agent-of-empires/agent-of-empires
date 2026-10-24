import { useWebSettings } from "../../hooks/useWebSettings";
import { CheckboxRow } from "./FormFields";

/// Per-browser pane auto-open defaults; they apply to sessions opened afterwards.
export function PanelsSettings() {
  const { settings, update } = useWebSettings();

  return (
    <div>
      <div className="space-y-4">
        <CheckboxRow
          title="Edit project text files"
          description="Allow editing existing UTF-8 text files in the Files pane. Saves refuse changes made on disk since opening."
          checked={settings.fileEditing}
          onChange={(v) => update({ fileEditing: v })}
        />
        <CheckboxRow
          title="Open the diff panel in new sessions"
          description="Show the diff pane automatically when a session first opens. Turn off to start with it closed; open it any time from the activity bar."
          checked={settings.autoOpenDiffPane}
          onChange={(v) => update({ autoOpenDiffPane: v })}
        />
        <CheckboxRow
          title="Open a terminal panel in new sessions"
          description="Show a terminal pane automatically when a session first opens. Turn off to start with it closed."
          checked={settings.autoOpenTerminalPane}
          onChange={(v) => update({ autoOpenTerminalPane: v })}
        />
        <CheckboxRow
          title="Open plugin panels automatically"
          description="Auto-open panes contributed by installed plugins, such as the GitHub pull-request pane. Turn off to keep them closed until you open one from the activity bar."
          checked={settings.autoOpenPluginPanes}
          onChange={(v) => update({ autoOpenPluginPanes: v })}
        />
      </div>
    </div>
  );
}

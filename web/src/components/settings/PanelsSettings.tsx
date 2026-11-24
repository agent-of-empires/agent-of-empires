import { useWebSettings } from "../../hooks/useWebSettings";
import { CheckboxRow } from "./FormFields";

/// Per-browser Panels preferences; the auto-open defaults are Daemon settings rendered beside these.
export function PanelsSettings() {
  const { settings, update } = useWebSettings();

  return (
    <CheckboxRow
      title="Edit project text files"
      description="Allow editing existing UTF-8 text files in the Files pane. Saves refuse changes made on disk since opening."
      checked={settings.fileEditing}
      onChange={(v) => update({ fileEditing: v })}
    />
  );
}

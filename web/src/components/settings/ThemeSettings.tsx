import { useEffect, useState } from "react";
import { fetchThemes } from "../../lib/api";
import { dispatchThemePickerChanged } from "../../hooks/useResolvedTheme";
import { SelectField } from "./FormFields";

interface Props {
  settings: Record<string, unknown>;
  onSaveField: (
    section: string,
    field: string,
    value: unknown,
  ) => Promise<boolean> | void;
  onUpdate: (patch: Record<string, unknown>) => void;
}

export function ThemeSettings({ settings, onSaveField, onUpdate }: Props) {
  const [themes, setThemes] = useState<string[]>([]);
  const theme = (settings.theme ?? {}) as Record<string, unknown>;

  useEffect(() => {
    fetchThemes().then(setThemes);
  }, []);

  const save = async (field: string, value: unknown) => {
    onUpdate({ theme: { ...theme, [field]: value } });
    const result = onSaveField("theme", field, value);
    // Only repaint the dashboard chrome after the PATCH lands.
    // Previously the picker dispatched the repaint event before
    // awaiting the PATCH, so a failed save (passphrase elevation
    // missing, read-only mode, network) left the dashboard painted
    // with a theme that wasn't on disk; a reload then snapped back
    // to the previous theme and looked like a silent revert.
    // See #1510.
    if (field !== "name") return;
    const ok = result instanceof Promise ? await result : true;
    if (!ok) return;
    dispatchThemePickerChanged(typeof value === "string" ? value : undefined);
  };

  return (
    <div className="space-y-4">
      <p className="text-xs text-text-dim">
        Theme applies to both the TUI and the web dashboard. Custom themes
        in <code>~/.agent-of-empires/themes/*.toml</code> appear alongside
        the builtins.
      </p>
      <SelectField
        label="Theme"
        value={(theme.name as string) ?? ""}
        onChange={(v) => save("name", v)}
        options={themes.map((t) => ({ value: t, label: t }))}
      />
      <SelectField
        label="Color mode"
        value={(theme.color_mode as string) ?? "truecolor"}
        onChange={(v) => save("color_mode", v)}
        options={[
          { value: "truecolor", label: "Truecolor (24-bit RGB)" },
          { value: "palette", label: "Palette (256 colors)" },
        ]}
      />
      <p className="text-xs text-text-dim">
        Color mode only affects the TUI; the web dashboard always uses
        truecolor.
      </p>
    </div>
  );
}

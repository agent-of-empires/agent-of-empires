import { useMemo } from "react";
import { updateSettings } from "../../lib/api";
import type { SettingsFieldDescriptor } from "../../lib/types";
import { SchemaSection } from "./SchemaSection";

const PLUGIN_PREFIX = "plugin:";

interface Props {
  /** Includes the virtual `plugin:<id>` sections. */
  schema: SettingsFieldDescriptor[];
  settings: Record<string, unknown> | null;
  onSaved: () => void;
}

function storedSettings(settings: Record<string, unknown> | null, id: string): Record<string, unknown> {
  const plugins = (settings?.plugins ?? {}) as Record<string, { settings?: Record<string, unknown> }>;
  return plugins[id]?.settings ?? {};
}

/** One SchemaSection per active plugin; plugin settings are global, saved via `PATCH /api/settings`. */
export function PluginSettingsSections({ schema, settings, onSaved }: Props) {
  const sections = useMemo(() => {
    const seen = new Set<string>();
    const ordered: string[] = [];
    for (const d of schema) {
      if (d.section.startsWith(PLUGIN_PREFIX) && !seen.has(d.section)) {
        seen.add(d.section);
        ordered.push(d.section);
      }
    }
    return ordered;
  }, [schema]);

  if (sections.length === 0) return null;

  const save = async (section: string, field: string, value: unknown): Promise<boolean> => {
    const ok = await updateSettings({ [section]: { [field]: value } });
    if (ok) onSaved();
    return ok;
  };

  return (
    <div className="space-y-6">
      <h4 className="text-xs font-mono uppercase tracking-widest text-text-muted">Plugin Settings</h4>
      {sections.map((section) => {
        const id = section.slice(PLUGIN_PREFIX.length);
        // Seed manifest defaults for fields with no stored value yet.
        const values: Record<string, unknown> = {};
        for (const d of schema) {
          if (d.section === section && d.default !== undefined) values[d.field] = d.default;
        }
        Object.assign(values, storedSettings(settings, id));
        return (
          <div key={section} className="space-y-3">
            <h5 className="text-xs font-mono text-text-secondary">{id}</h5>
            <SchemaSection section={section} schema={schema} values={values} onSaveField={save} />
          </div>
        );
      })}
    </div>
  );
}

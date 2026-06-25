import { useMemo } from "react";
import { updateSettings } from "../../lib/api";
import type { SettingsFieldDescriptor } from "../../lib/types";
import { SchemaSection } from "./SchemaSection";

const PLUGIN_PREFIX = "plugin:";

interface Props {
  /** Full schema descriptor list (`GET /api/settings/schema`), including the
   *  virtual `plugin:<id>` sections of active plugins. */
  schema: SettingsFieldDescriptor[];
  /** The loaded global settings object (`GET /api/settings`). */
  settings: Record<string, unknown> | null;
  /** Re-fetch settings after a successful save, so the new value round-trips. */
  onSaved: () => void;
}

/** Stored value table for a plugin's settings: `plugins.<id>.settings`. */
function storedSettings(settings: Record<string, unknown> | null, id: string): Record<string, unknown> {
  const plugins = (settings?.plugins ?? {}) as Record<string, { settings?: Record<string, unknown> }>;
  return plugins[id]?.settings ?? {};
}

/**
 * Settings for active plugins, rendered through the same generic SchemaSection
 * as core settings, one block per `plugin:<id>` section. Plugin settings are
 * global-only at Tier 0, so saves go through the global `PATCH /api/settings`
 * (the server folds `plugin:<id>` into `plugins.<id>.settings`); the manifest's
 * declared default is shown until a value is stored.
 */
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

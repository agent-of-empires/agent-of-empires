import { useMemo } from "react";
import type { SettingsFieldDescriptor } from "../../lib/types";
import { SchemaSection } from "./SchemaSection";
import { SectionHeader } from "./suggestionsShared";

const PLUGIN_PREFIX = "plugin:";
/** Plugin settings with this key prefix also show in Settings > Suggestions. */
const SUGGESTION_PREFIX = "suggestion_";

interface Props {
  /** Includes the virtual `plugin:<id>` sections. */
  schema: SettingsFieldDescriptor[];
  settings: Record<string, unknown> | null;
  /** Saves through the settings store, which keeps edits across remounts and retries uncertain ones. */
  onSaveField: (section: string, field: string, value: unknown) => Promise<boolean>;
}

/** Manifest defaults overlaid with the stored plugin settings. */
function pluginValues(
  schema: SettingsFieldDescriptor[],
  settings: Record<string, unknown> | null,
  id: string,
): Record<string, unknown> {
  const values: Record<string, unknown> = {};
  for (const d of schema) {
    if (d.section === PLUGIN_PREFIX + id && d.default !== undefined) values[d.field] = d.default;
  }
  const plugins = (settings?.plugins ?? {}) as Record<string, { settings?: Record<string, unknown> }>;
  return Object.assign(values, plugins[id]?.settings);
}

/** One SchemaSection per active plugin; plugin settings are global, saved via `PATCH /api/settings`. */
export function PluginSettingsSections({ schema, settings, onSaveField }: Props) {
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

  return (
    <div className="space-y-6">
      <h4 className="text-xs font-mono uppercase tracking-widest text-text-muted">Plugin Settings</h4>
      {sections.map((section) => {
        const id = section.slice(PLUGIN_PREFIX.length);
        const values = pluginValues(schema, settings, id);
        return (
          <div key={section} className="space-y-3">
            <h5 className="text-xs font-mono text-text-secondary">{id}</h5>
            <SchemaSection section={section} schema={schema} values={values} onSaveField={onSaveField} />
          </div>
        );
      })}
    </div>
  );
}

/** One plugin's `suggestion_*` settings, unfolded, saved like on the Plugins tab. */
export function PluginSuggestionSettings({
  pluginId,
  title,
  schema,
  settings,
  onSaveField,
}: Props & { pluginId: string; title: string }) {
  const section = PLUGIN_PREFIX + pluginId;
  const fields = useMemo(
    () =>
      schema
        .filter((d) => d.section === section && d.field.startsWith(SUGGESTION_PREFIX))
        .map((d) => ({ ...d, advanced: false })),
    [schema, section],
  );
  if (fields.length === 0) return null;
  return (
    <section aria-label={title}>
      <SectionHeader>{title}</SectionHeader>
      <SchemaSection
        section={section}
        schema={fields}
        values={pluginValues(schema, settings, pluginId)}
        onSaveField={onSaveField}
      />
    </section>
  );
}

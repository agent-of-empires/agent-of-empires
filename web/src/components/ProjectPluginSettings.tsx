import { useEffect, useMemo, useState } from "react";
import { fetchMachineSettings, getSettingsSchema } from "../lib/api";
import type { SettingsFieldDescriptor } from "../lib/types";
import { SchemaSection } from "./settings/SchemaSection";

const PLUGIN_PREFIX = "plugin:";

/** Unsaved edits per plugin id and setting key; `null` clears the project's value. */
export type PluginSettingsDraft = Record<string, Record<string, unknown>>;

interface Props {
  /** The project's saved per-project plugin values. */
  stored?: Record<string, Record<string, unknown>>;
  draft: PluginSettingsDraft;
  onChange: (draft: PluginSettingsDraft) => void;
}

function globalValues(settings: Record<string, unknown> | null, id: string): Record<string, unknown> {
  const plugins = (settings?.plugins ?? {}) as Record<string, { settings?: Record<string, unknown> }>;
  return plugins[id]?.settings ?? {};
}

/** The project's own value for a key: a pending edit wins over the saved one, and `null` clears it. */
function projectValue(
  stored: Props["stored"],
  draft: PluginSettingsDraft,
  id: string,
  field: string,
): { value: unknown; pending: boolean } {
  const pending = draft[id]?.[field];
  const value = pending !== undefined ? (pending ?? undefined) : stored?.[id]?.[field];
  return { value, pending: pending !== undefined };
}

/** Per-project values of the plugin settings that declare `per_project`, falling back to the global value. */
export function ProjectPluginSettings({ stored, draft, onChange }: Props) {
  const [schema, setSchema] = useState<SettingsFieldDescriptor[]>([]);
  const [settings, setSettings] = useState<Record<string, unknown> | null>(null);

  useEffect(() => {
    let live = true;
    void Promise.all([getSettingsSchema(), fetchMachineSettings()]).then(([s, g]) => {
      if (!live) return;
      setSchema((s ?? []).filter((d) => d.section.startsWith(PLUGIN_PREFIX)));
      setSettings(g as Record<string, unknown> | null);
    });
    return () => {
      live = false;
    };
  }, []);

  // Every field of a plugin that has a per-project one: dependent controls read siblings, which may be global-only.
  const plugins = useMemo(() => {
    const byPlugin = new Map<string, SettingsFieldDescriptor[]>();
    for (const d of schema) {
      const id = d.section.slice(PLUGIN_PREFIX.length);
      byPlugin.set(id, [...(byPlugin.get(id) ?? []), d]);
    }
    return [...byPlugin].filter(([, fields]) => fields.some((d) => d.per_project));
  }, [schema]);

  if (plugins.length === 0) return null;

  const setKey = (id: string, key: string, value: unknown | undefined) => {
    const { [key]: _dropped, ...rest } = draft[id] ?? {};
    const next = value === undefined ? rest : { ...rest, [key]: value };
    const { [id]: _plugin, ...others } = draft;
    onChange(Object.keys(next).length > 0 ? { ...others, [id]: next } : others);
  };

  return (
    <div className="mb-4" data-testid="project-plugin-settings">
      <h3 className="text-[12px] text-text-dim mb-2">Plugin settings</h3>
      {plugins.map(([id, fields]) => {
        const global = globalValues(settings, id);
        // The effective section as this project sees it, so a control resolves dependencies from it.
        const effective: Record<string, unknown> = {};
        for (const d of fields) {
          const own = d.per_project ? projectValue(stored, draft, id, d.field).value : undefined;
          const value = own ?? global[d.field] ?? d.default;
          if (value !== undefined) effective[d.field] = value;
        }
        return (
          <div key={id} className="mb-3 space-y-3">
            <h4 className="text-xs font-mono text-text-secondary">{id}</h4>
            {fields
              .filter((d) => d.per_project)
              .map((d) => {
                const overridden = projectValue(stored, draft, id, d.field).value !== undefined;
                const savedValue = stored?.[id]?.[d.field];
                return (
                  <div key={d.field} data-testid={`project-plugin-setting-${id}.${d.field}`}>
                    <SchemaSection
                      section={d.section}
                      schema={fields.map((f) => ({ ...f, profile_overridable: true }))}
                      values={effective}
                      onlyFields={[d.field]}
                      onSaveField={(_section, field, value) => {
                        setKey(id, field, value);
                        return true;
                      }}
                    />
                    <div className="mt-1 flex items-center gap-2 text-[11px] text-text-dim">
                      <span>{overridden ? "Set for this project" : "Using the global value"}</span>
                      {overridden && (
                        <button
                          type="button"
                          onClick={() => setKey(id, d.field, savedValue !== undefined ? null : undefined)}
                          className="underline cursor-pointer hover:text-text-secondary"
                        >
                          Use global
                        </button>
                      )}
                    </div>
                  </div>
                );
              })}
          </div>
        );
      })}
    </div>
  );
}

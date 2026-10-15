import { useEffect, useMemo, useSyncExternalStore } from "react";
import { dropOverlay, getPendingSaves, queueSave, setPending, subscribePendingSaves } from "./pluginSaves";
import { updateSettings } from "../../lib/api";
import type { SettingsFieldDescriptor } from "../../lib/types";
import { SchemaSection } from "./SchemaSection";

const PLUGIN_PREFIX = "plugin:";

interface Props {
  /** Includes the virtual `plugin:<id>` sections. */
  schema: SettingsFieldDescriptor[];
  settings: Record<string, unknown> | null;
  /** Refetches the settings; resolves once a fetch started after the save has landed. */
  onSaved: () => Promise<void> | void;
}

function storedSettings(settings: Record<string, unknown> | null, id: string): Record<string, unknown> {
  const plugins = (settings?.plugins ?? {}) as Record<string, { settings?: Record<string, unknown> }>;
  return plugins[id]?.settings ?? {};
}

/** One SchemaSection per active plugin; plugin settings are global, saved via `PATCH /api/settings`. */
export function PluginSettingsSections({ schema, settings, onSaved }: Props) {
  const pending = useSyncExternalStore(subscribePendingSaves, getPendingSaves);
  // An overlay outlives remounts and is dropped only once the settings show its value; a fetch that
  // still shows the old value predates the commit (or the write is still on its way).
  useEffect(() => {
    const keep = Object.entries(pending).filter(
      ([, e]) =>
        e.inflight > 0 ||
        JSON.stringify(storedSettings(settings, e.section.slice(PLUGIN_PREFIX.length))[e.field]) !==
          JSON.stringify(e.value),
    );
    if (keep.length !== Object.keys(pending).length) setPending(() => Object.fromEntries(keep));
  }, [settings, pending]);

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

  // Apply the value at once, like the core settings tabs, so an editor that acts on
  // `values` again before the PATCH and refetch finish builds on this save, not the old value.
  const save = async (section: string, field: string, value: unknown): Promise<boolean> => {
    const key = `${section}\0${field}`;
    setPending((p) => ({ ...p, [key]: { ...p[key], section, field, value, inflight: (p[key]?.inflight ?? 0) + 1 } }));
    // The server applies each PATCH atomically but not in arrival order, so send a field's saves one at a time.
    const request = queueSave(key, () => updateSettings({ [section]: { [field]: value } }));
    const outcome = await request;
    setPending((p) => {
      const entry = p[key];
      if (!entry) return p;
      const inflight = entry.inflight - 1;
      const acked = outcome === "committed" ? { value } : entry.acked;
      if (outcome === "rejected" && inflight === 0) {
        // Fall back to what the server last accepted: the settings prop may not have refetched it yet.
        if (!acked) return Object.fromEntries(Object.entries(p).filter(([k]) => k !== key));
        return { ...p, [key]: { ...entry, value: acked.value, inflight, acked } };
      }
      return { ...p, [key]: { ...entry, inflight, acked } };
    });
    // An uncertain save keeps its value on screen; the refetch settles what the server holds.
    if (outcome !== "rejected") {
      void Promise.resolve(onSaved()).then(() => {
        // The overlay only bridges until a fetch that started after the commit lands.
        if (outcome === "committed") dropOverlay(key);
      });
    }
    return outcome !== "rejected";
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
        for (const e of Object.values(pending)) {
          if (e.section === section) values[e.field] = e.value;
        }
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

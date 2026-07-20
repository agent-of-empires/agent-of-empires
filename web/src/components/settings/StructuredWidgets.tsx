// API v9 structured plugin settings widgets (#2897): dynamic_select (host
// option source), cron (validated text), and object_list (repeatable
// structured items). Rendered generically from the schema, so any plugin's
// object_list of dynamic_selects works without plugin-specific host code.

import { useEffect, useRef, useState } from "react";
import { resolvePluginOptions } from "../../lib/api";
import type { SettingsObjectField, SettingsObjectFieldWidget, SettingsOptionSource } from "../../lib/types";
import { validateCron } from "./cronValidation";
import { NumberField, SelectField, TextField, ToggleField } from "./FormFields";

/** Plugin id embedded in a `plugin:<id>` section id. */
function pluginIdOf(section: string): string {
  return section.startsWith("plugin:") ? section.slice("plugin:".length) : section;
}

/** A stable item id. Uses crypto.randomUUID in a secure browser context and
 *  falls back to a random string where it is unavailable (older webviews,
 *  jsdom); the host only requires a non-empty unique string, not a real UUID. */
function newItemId(): string {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  return `id-${Math.random().toString(36).slice(2)}-${Date.now().toString(36)}`;
}

/** A select whose options the host resolves for a `dynamic_select`. Reloads
 *  when its dependency values change; preserves a stored value that is no
 *  longer offered by showing it as "(unavailable)" rather than dropping it,
 *  since it may still be valid (sessions.create is the authoritative check). */
function PluginOptionSelect({
  label,
  description,
  section,
  source,
  depends,
  value,
  onChange,
}: {
  label: string;
  description?: string;
  section: string;
  source: SettingsOptionSource;
  depends: string[];
  value: string;
  onChange: (v: string) => void;
}) {
  const [options, setOptions] = useState<{ value: string; label: string }[]>([]);
  const depsKey = depends.join("");
  // A monotonically-increasing request id so a slow earlier resolve cannot
  // overwrite a newer one (dependency changed while a fetch was in flight).
  const reqId = useRef(0);

  useEffect(() => {
    const id = ++reqId.current;
    resolvePluginOptions(pluginIdOf(section), source, depends).then((opts) => {
      if (id === reqId.current) setOptions(opts);
    });
    // depsKey captures the dependency values; source/section are stable.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [section, source, depsKey]);

  const known = options.some((o) => o.value === value);
  const shown = value && !known ? [{ value, label: `${value} (unavailable)` }, ...options] : options;
  // An empty placeholder so a required-but-unset field does not silently adopt
  // the first option.
  const withPlaceholder = value ? shown : [{ value: "", label: "Select..." }, ...shown];

  return (
    <SelectField label={label} description={description} value={value} onChange={onChange} options={withPlaceholder} />
  );
}

/** A cron text field with live client-side validation feedback. The server is
 *  authoritative; this is a UX nicety mirroring the same 5-field grammar. */
export function CronField({
  label,
  description,
  value,
  onChange,
}: {
  label: string;
  description?: string;
  value: string;
  onChange: (v: string) => void;
}) {
  const error = value ? validateCron(value) : null;
  return (
    <div>
      <TextField
        label={label}
        description={description}
        value={value}
        onChange={onChange}
        mono
        placeholder="0 9 * * 1-5"
      />
      {error && <div className="text-xs text-status-error mt-1">{error}</div>}
    </div>
  );
}

/** Top-level `dynamic_select` field: resolves `depends_on` sibling values from
 *  the section's current values. */
export function DynamicSelectField({
  label,
  description,
  section,
  source,
  dependsOn,
  sectionValues,
  value,
  onChange,
}: {
  label: string;
  description?: string;
  section: string;
  source: SettingsOptionSource;
  dependsOn: string[];
  sectionValues: Record<string, unknown>;
  value: string;
  onChange: (v: string) => void;
}) {
  const depends = dependsOn.map((k) => {
    const v = sectionValues[k];
    return typeof v === "string" ? v : "";
  });
  return (
    <PluginOptionSelect
      label={label}
      description={description}
      section={section}
      source={source}
      depends={depends}
      value={value}
      onChange={onChange}
    />
  );
}

type Item = Record<string, unknown>;

/** Render one nested object-list item field into the matching control. */
function renderItemField(
  section: string,
  field: SettingsObjectField,
  item: Item,
  setField: (key: string, value: unknown) => void,
) {
  const widget: SettingsObjectFieldWidget = field.widget;
  const raw = item[field.field];
  switch (widget.kind) {
    case "toggle":
      return (
        <ToggleField
          key={field.field}
          label={field.label}
          description={field.description}
          checked={typeof raw === "boolean" ? raw : false}
          onChange={(v) => setField(field.field, v)}
        />
      );
    case "number":
      return (
        <NumberField
          key={field.field}
          label={field.label}
          description={field.description}
          value={typeof raw === "number" ? raw : 0}
          onChange={(v) => setField(field.field, v)}
          min={widget.min}
          max={widget.max}
        />
      );
    case "select":
      return (
        <SelectField
          key={field.field}
          label={field.label}
          description={field.description}
          value={typeof raw === "string" ? raw : ""}
          onChange={(v) => setField(field.field, v)}
          options={widget.options}
        />
      );
    case "cron":
      return (
        <CronField
          key={field.field}
          label={field.label}
          description={field.description}
          value={typeof raw === "string" ? raw : ""}
          onChange={(v) => setField(field.field, v)}
        />
      );
    case "dynamic_select":
      return (
        <PluginOptionSelect
          key={field.field}
          label={field.label}
          description={field.description}
          section={section}
          source={widget.source}
          depends={(widget.depends_on ?? []).map((k) => {
            const v = item[k];
            return typeof v === "string" ? v : "";
          })}
          value={typeof raw === "string" ? raw : ""}
          onChange={(v) => setField(field.field, v)}
        />
      );
    case "text":
      return (
        <TextField
          key={field.field}
          label={field.label}
          description={field.description}
          value={typeof raw === "string" ? raw : ""}
          onChange={(v) => setField(field.field, v)}
          mono={widget.mono}
          multiline={widget.multiline}
        />
      );
  }
}

/** A repeatable list of structured items with add / remove / move controls.
 *  Each item carries a stable id under `idField`, generated on add and never
 *  regenerated on edit or reorder, so a dependent worker can track it. */
export function ObjectListField({
  label,
  description,
  section,
  idField,
  fields,
  minItems,
  maxItems,
  items,
  onChange,
}: {
  label: string;
  description?: string;
  section: string;
  idField: string;
  fields: SettingsObjectField[];
  minItems?: number;
  maxItems?: number;
  items: Item[];
  onChange: (items: Item[]) => void;
}) {
  // Local working copy so a newly added item with unfilled required fields can
  // be edited in place; we persist only once every item is valid, because the
  // server rejects an object_list whose required fields are empty (a bare
  // onChange would fail to save and the row could never enter edit state).
  // Re-sync when the persisted content changes (keyed by value, not identity, so
  // the empty-array fallback does not wipe in-progress drafts each render).
  // Adjusting state during render is React's recommended alternative to a
  // syncing effect.
  const [working, setWorking] = useState<Item[]>(items);
  const itemsKey = JSON.stringify(items);
  const [syncedKey, setSyncedKey] = useState(itemsKey);
  if (itemsKey !== syncedKey) {
    setSyncedKey(itemsKey);
    setWorking(items);
  }

  const itemValid = (it: Item) =>
    fields.every((f) => {
      if (!f.required) return true;
      const v = it[f.field];
      return typeof v === "string" ? v.trim() !== "" : v !== undefined && v !== null;
    });
  const commit = (next: Item[]) => {
    setWorking(next);
    // Persist only a fully valid list; incomplete new rows stay local until filled.
    if (next.every(itemValid)) onChange(next);
  };

  const setItem = (index: number, next: Item) => {
    commit(working.map((it, i) => (i === index ? next : it)));
  };
  const addItem = () => {
    const item: Item = { [idField]: newItemId() };
    for (const f of fields) {
      if (f.default !== undefined) item[f.field] = f.default;
    }
    commit([...working, item]);
  };
  const removeItem = (index: number) => commit(working.filter((_, i) => i !== index));
  const move = (index: number, delta: number) => {
    const target = index + delta;
    if (target < 0 || target >= working.length) return;
    const a = working[index];
    const b = working[target];
    if (a === undefined || b === undefined) return;
    const next = working.slice();
    next[index] = b;
    next[target] = a;
    commit(next);
  };

  const atMax = maxItems !== undefined && working.length >= maxItems;
  const atMin = minItems !== undefined && working.length <= minItems;

  return (
    <div className="space-y-2">
      <div>
        <div className="text-sm text-text-bright">{label}</div>
        {description && <div className="text-xs text-text-dim">{description}</div>}
      </div>
      {working.map((item, index) => {
        const id = String(item[idField] ?? index);
        const setField = (key: string, value: unknown) => setItem(index, { ...item, [key]: value });
        return (
          <div key={id} className="rounded-lg border border-surface-700 bg-surface-900 p-3 space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-xs text-text-dim">Item {index + 1}</span>
              <div className="flex gap-1">
                <button
                  type="button"
                  aria-label="Move up"
                  disabled={index === 0}
                  onClick={() => move(index, -1)}
                  className="px-2 py-0.5 text-xs text-text-dim hover:text-text-primary disabled:opacity-40"
                >
                  ↑
                </button>
                <button
                  type="button"
                  aria-label="Move down"
                  disabled={index === working.length - 1}
                  onClick={() => move(index, 1)}
                  className="px-2 py-0.5 text-xs text-text-dim hover:text-text-primary disabled:opacity-40"
                >
                  ↓
                </button>
                <button
                  type="button"
                  aria-label="Remove item"
                  disabled={atMin}
                  onClick={() => removeItem(index)}
                  className="px-2 py-0.5 text-xs text-status-error hover:opacity-80 disabled:opacity-40"
                >
                  Remove
                </button>
              </div>
            </div>
            {fields.map((f) => renderItemField(section, f, item, setField))}
          </div>
        );
      })}
      <button
        type="button"
        disabled={atMax}
        onClick={addItem}
        className="px-3 py-1.5 text-sm rounded-md border border-surface-700 text-text-primary hover:border-brand-600 disabled:opacity-40"
      >
        Add item
      </button>
    </div>
  );
}

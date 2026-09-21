// Structured plugin settings widgets rendered generically from the schema.

import { useEffect, useRef, useState } from "react";
import { resolvePluginOptions } from "../../lib/api";
import type { SettingsObjectField, SettingsOptionSource } from "../../lib/types";
import { validateCron } from "./cronValidation";
import { NumberField, SelectField, TextField, ToggleField } from "./FormFields";

type Option = { value: string; label: string };
type Item = Record<string, unknown>;

const str = (v: unknown) => (typeof v === "string" ? v : "");
const strings = (obj: Record<string, unknown>, keys: string[]) => keys.map((k) => str(obj[k]));

/** A stable item id; the host only needs a unique non-empty string. */
function newItemId(): string {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  return `id-${Math.random().toString(36).slice(2)}-${Date.now().toString(36)}`;
}

/** Host-resolved options, refetched when dependency values change; stale responses are dropped. */
function useResolvedOptions(section: string, source: SettingsOptionSource, depends: string[]): Option[] {
  const [options, setOptions] = useState<Option[]>([]);
  // Unit separator: dependency values such as paths may contain spaces.
  const depsKey = depends.join("");
  const reqId = useRef(0);
  useEffect(() => {
    const id = ++reqId.current;
    const pluginId = section.startsWith("plugin:") ? section.slice("plugin:".length) : section;
    resolvePluginOptions(pluginId, source, depends).then((opts) => {
      if (id === reqId.current) setOptions(opts);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [section, source, depsKey]);
  return options;
}

interface OptionProps {
  label: string;
  description?: string;
  section: string;
  source: SettingsOptionSource;
  depends: string[];
}

/** A stored value no longer offered stays visible as "(unavailable)"; the server is authoritative. */
function PluginOptionSelect({
  value,
  onChange,
  ...props
}: OptionProps & { value: string; onChange: (v: string) => void }) {
  const options = useResolvedOptions(props.section, props.source, props.depends);
  const known = options.some((o) => o.value === value);
  const shown = value && !known ? [{ value, label: `${value} (unavailable)` }, ...options] : options;
  // A placeholder keeps an unset field from silently adopting the first option.
  const withPlaceholder = value ? shown : [{ value: "", label: "Select..." }, ...shown];
  return (
    <SelectField
      label={props.label}
      description={props.description}
      value={value}
      onChange={onChange}
      options={withPlaceholder}
    />
  );
}

function PluginOptionMultiSelect({
  values,
  onChange,
  ...props
}: OptionProps & { values: string[]; onChange: (v: string[]) => void }) {
  const options = useResolvedOptions(props.section, props.source, props.depends);
  const known = new Set(options.map((o) => o.value));
  const shown = [
    ...options,
    ...values.filter((v) => !known.has(v)).map((v) => ({ value: v, label: `${v} (unavailable)` })),
  ];
  const toggle = (val: string) => onChange(values.includes(val) ? values.filter((v) => v !== val) : [...values, val]);

  return (
    <div>
      <div className="text-sm text-text-bright">{props.label}</div>
      {props.description && <div className="text-xs text-text-dim">{props.description}</div>}
      <div className="mt-1 space-y-1">
        {shown.length === 0 && <div className="text-xs text-text-dim">No options available.</div>}
        {shown.map((o) => (
          <label key={o.value} className="flex items-center gap-2 text-sm text-text-primary">
            <input type="checkbox" checked={values.includes(o.value)} onChange={() => toggle(o.value)} />
            {o.label}
          </label>
        ))}
      </div>
    </div>
  );
}

/** Cron text with client-side validation feedback. */
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

/** Top-level `dynamic_select`, resolving `depends_on` from sibling section values. */
export function DynamicSelectField({
  dependsOn,
  sectionValues,
  ...props
}: Omit<OptionProps, "depends"> & {
  dependsOn: string[];
  sectionValues: Record<string, unknown>;
  value: string;
  onChange: (v: string) => void;
}) {
  return <PluginOptionSelect {...props} depends={strings(sectionValues, dependsOn)} />;
}

function renderItemField(section: string, field: SettingsObjectField, item: Item, setField: (v: unknown) => void) {
  const { widget } = field;
  const raw = item[field.field];
  const common = { label: field.label, description: field.description };
  switch (widget.kind) {
    case "toggle":
      return (
        <ToggleField
          key={field.field}
          {...common}
          checked={typeof raw === "boolean" ? raw : false}
          onChange={setField}
        />
      );
    case "number":
      return (
        <NumberField
          key={field.field}
          {...common}
          value={typeof raw === "number" ? raw : 0}
          onChange={setField}
          min={widget.min}
          max={widget.max}
        />
      );
    case "select":
      return (
        <SelectField key={field.field} {...common} value={str(raw)} onChange={setField} options={widget.options} />
      );
    case "cron":
      return <CronField key={field.field} {...common} value={str(raw)} onChange={setField} />;
    case "dynamic_select":
      return (
        <PluginOptionSelect
          key={field.field}
          {...common}
          section={section}
          source={widget.source}
          depends={strings(item, widget.depends_on ?? [])}
          value={str(raw)}
          onChange={setField}
        />
      );
    case "dynamic_multi_select":
      return (
        <PluginOptionMultiSelect
          key={field.field}
          {...common}
          section={section}
          source={widget.source}
          depends={strings(item, widget.depends_on ?? [])}
          values={Array.isArray(raw) ? raw.filter((x): x is string => typeof x === "string") : []}
          onChange={setField}
        />
      );
    case "text":
      return (
        <TextField
          key={field.field}
          {...common}
          value={str(raw)}
          onChange={setField}
          mono={widget.mono}
          multiline={widget.multiline}
        />
      );
  }
}

/** Repeatable structured items. Each item keeps the id under `idField` generated on add. */
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
  // The server rejects items with empty required fields, so incomplete rows stay
  // in this working copy until valid. Re-sync by content, not identity.
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
    if (next.every(itemValid)) onChange(next);
  };
  const addItem = () => {
    const item: Item = { [idField]: newItemId() };
    for (const f of fields) {
      if (f.default !== undefined) item[f.field] = f.default;
    }
    commit([...working, item]);
  };
  const move = (index: number, delta: number) => {
    const target = index + delta;
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
  const itemButtons = (index: number) => [
    { label: "Move up", text: "↑", disabled: index === 0, onClick: () => move(index, -1) },
    { label: "Move down", text: "↓", disabled: index === working.length - 1, onClick: () => move(index, 1) },
  ];

  return (
    <div className="space-y-2">
      <div>
        <div className="text-sm text-text-bright">{label}</div>
        {description && <div className="text-xs text-text-dim">{description}</div>}
      </div>
      {working.map((item, index) => (
        <div
          key={String(item[idField] ?? index)}
          className="rounded-lg border border-surface-700 bg-surface-900 p-3 space-y-2"
        >
          <div className="flex items-center justify-between">
            <span className="text-xs text-text-dim">Item {index + 1}</span>
            <div className="flex gap-1">
              {itemButtons(index).map((b) => (
                <button
                  key={b.label}
                  type="button"
                  aria-label={b.label}
                  disabled={b.disabled}
                  onClick={b.onClick}
                  className="px-2 py-0.5 text-xs text-text-dim hover:text-text-primary disabled:opacity-40"
                >
                  {b.text}
                </button>
              ))}
              <button
                type="button"
                aria-label="Remove item"
                disabled={atMin}
                onClick={() => commit(working.filter((_, i) => i !== index))}
                className="px-2 py-0.5 text-xs text-status-error hover:opacity-80 disabled:opacity-40"
              >
                Remove
              </button>
            </div>
          </div>
          {fields.map((f) =>
            renderItemField(section, f, item, (value) =>
              commit(working.map((it, i) => (i === index ? { ...item, [f.field]: value } : it))),
            ),
          )}
        </div>
      ))}
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

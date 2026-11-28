// Structured plugin settings widgets rendered generically from the schema.

import { useEffect, useRef, useState } from "react";
import { resolvePluginOptions } from "../../lib/api";
import type { SettingsObjectField, SettingsOptionSource } from "../../lib/types";
import { BUTTON_DANGER_GHOST_SM, BUTTON_GHOST_SM, FIELD_DESCRIPTION, FIELD_LABEL, INSET } from "../../lib/surfaces";
import { PanelMessage } from "../panelParts";
import { BUTTON_SECONDARY } from "../../lib/surfaces";
import { validateCron } from "./cronValidation";
import { ListField, NumberField, SelectField, TextField, ToggleField } from "./FormFields";
import type { RowLabel } from "./rowLabel";

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
  row?: RowLabel;
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
      row={props.row}
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
      <div className={props.description ? FIELD_LABEL : FIELD_LABEL.replace("mb-0.5", "mb-2")}>{props.label}</div>
      {props.description && <div className={FIELD_DESCRIPTION}>{props.description}</div>}
      <div className="space-y-1">
        {shown.length === 0 && <PanelMessage>No options available.</PanelMessage>}
        {shown.map((o) => (
          <label
            key={o.value}
            className="flex min-h-7 max-md:min-h-11 cursor-pointer items-center gap-2 text-[15px] md:text-sm text-text-primary"
          >
            <input
              type="checkbox"
              checked={values.includes(o.value)}
              onChange={() => toggle(o.value)}
              className="accent-brand-600 w-4 h-4 shrink-0"
            />
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
  row,
  value,
  onChange,
}: {
  label: string;
  description?: string;
  row?: RowLabel;
  value: string;
  onChange: (v: string) => void;
}) {
  const error = value ? validateCron(value) : null;
  return (
    <div>
      <TextField
        label={label}
        description={description}
        row={row}
        value={value}
        onChange={onChange}
        mono
        placeholder="0 9 * * 1-5"
      />
      {error && <div className="text-[13px] text-status-error-text mt-1">{error}</div>}
    </div>
  );
}

/** Top-level `dynamic_select`, resolving `depends_on` from sibling section values. */
export function DynamicSelectField({
  dependsOn,
  sectionValues,
  ...props
}: Omit<OptionProps, "depends" | "label" | "description" | "row"> & {
  row: RowLabel;
  dependsOn: string[];
  sectionValues: Record<string, unknown>;
  value: string;
  onChange: (v: string) => void;
}) {
  return <PluginOptionSelect {...props} label="" depends={strings(sectionValues, dependsOn)} />;
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
    case "list":
      return (
        <ListField
          key={field.field}
          {...common}
          items={Array.isArray(raw) ? raw.filter((x): x is string => typeof x === "string") : []}
          onChange={setField}
        />
      );
  }
}

/** Repeatable structured items. Each item keeps the id under `idField` generated on add. */
export function ObjectListField({
  row,
  section,
  idField,
  fields,
  minItems,
  maxItems,
  items,
  onChange,
}: {
  row: RowLabel;
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
      if (Array.isArray(v)) return v.length > 0;
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
    <div role="group" aria-labelledby={row.labelId} className="space-y-2">
      {working.map((item, index) => (
        <div key={String(item[idField] ?? index)} className={`${INSET} p-3 space-y-2`}>
          <div className="flex items-center justify-between">
            <span className="text-[13px] text-text-dim">Item {index + 1}</span>
            <div className="flex gap-1">
              {itemButtons(index).map((b) => (
                <button
                  key={b.label}
                  type="button"
                  aria-label={b.label}
                  disabled={b.disabled}
                  onClick={b.onClick}
                  className={BUTTON_GHOST_SM}
                >
                  {b.text}
                </button>
              ))}
              <button
                type="button"
                aria-label="Remove item"
                disabled={atMin}
                onClick={() => commit(working.filter((_, i) => i !== index))}
                className={BUTTON_DANGER_GHOST_SM}
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
      <button type="button" disabled={atMax} onClick={addItem} className={BUTTON_SECONDARY}>
        Add item
      </button>
    </div>
  );
}

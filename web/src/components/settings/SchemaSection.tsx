import { useEffect, useRef } from "react";
import type { SettingsFieldDescriptor, SettingsValidation } from "../../lib/types";
import { type TourAnchorId, tourAnchor } from "../../lib/tourSteps";
import {
  CollapsibleSection,
  ListField,
  NumberField,
  SelectField,
  SliderField,
  TextField,
  ToggleField,
} from "./FormFields";
import { CUSTOM_SETTINGS_WIDGETS } from "./customWidgetRegistry";
import { CronField, DynamicSelectField, ObjectListField } from "./StructuredWidgets";

interface Props {
  section: string;
  schema: SettingsFieldDescriptor[];
  values: Record<string, unknown>;
  /** Persist one field; `null` clears a profile override. May be sync or async. */
  onSaveField: (section: string, field: string, value: unknown) => unknown;
  advancedSubtitle?: string;
  /** Runs once after any field in this section saves successfully. */
  onAfterSave?: (descriptor: SettingsFieldDescriptor, value: unknown) => Promise<void> | void;
  /** A settings-search jump: scroll to and highlight the field, opening Advanced if needed. */
  focusRequest?: { section: string; field: string; nonce: number } | null;
  /** Tour anchor for one field's wrapper. */
  fieldAnchor?: { field: string; anchor: TourAnchorId };
  /** CityHall curation: an allowlist and a denylist of field names. */
  onlyFields?: string[];
  hideFields?: string[];
}

/** Client-side list-entry check mirroring the server rule. */
function listValidator(validation: SettingsValidation): ((value: string) => string | null) | undefined {
  switch (validation.rule) {
    case "volume_list":
      return (v) => (v.includes(":") ? null : "Must contain ':' (host:container)");
    case "env_list":
      return (v) =>
        /^[A-Za-z_][A-Za-z0-9_]*(=.*)?$/.test(v) ? null : "Must be KEY or KEY=VALUE (letters, digits, underscores)";
    case "port_mapping_list":
      return (v) => (/^\d+:\d+$/.test(v) ? null : "Must be port:port (e.g. 3000:3000)");
    default:
      return undefined;
  }
}

function describe(d: SettingsFieldDescriptor): string {
  if (d.profile_overridable) return d.description;
  const note = "Applies to all profiles (not profile-overridable).";
  return d.description ? `${d.description} ${note}` : note;
}

/** Keeps a schema/web mismatch visible instead of silently dropping the field. */
function UnsupportedCustomWidget({ d, id }: { d: SettingsFieldDescriptor; id: string }) {
  return (
    <div className="text-xs text-status-error bg-status-error/10 rounded-lg p-3">
      No web control registered for "{d.label}" (custom widget "{id}"). Edit it from the TUI or <code>config.toml</code>
      .
    </div>
  );
}

function renderField(
  d: SettingsFieldDescriptor,
  values: Record<string, unknown>,
  save: (value: unknown) => Promise<boolean>,
) {
  const raw = values[d.field];
  const widget = d.widget;
  const common = { label: d.label, description: describe(d) };
  const str = typeof raw === "string" ? raw : "";

  switch (widget.kind) {
    case "toggle":
      return <ToggleField key={d.field} {...common} checked={typeof raw === "boolean" ? raw : false} onChange={save} />;
    case "text":
      return (
        <TextField
          key={d.field}
          {...common}
          value={str}
          onChange={(v) => save(v)}
          mono={widget.mono}
          multiline={widget.multiline}
        />
      );
    case "optional_text":
      return <TextField key={d.field} {...common} value={str} onChange={(v) => save(v || null)} mono={widget.mono} />;
    case "number":
      return (
        <NumberField
          key={d.field}
          {...common}
          value={typeof raw === "number" ? raw : 0}
          onChange={save}
          min={widget.min}
          max={widget.max}
        />
      );
    case "slider":
      return (
        <SliderField
          key={d.field}
          {...common}
          value={typeof raw === "number" ? raw : widget.min}
          onChange={save}
          min={widget.min}
          max={widget.max}
          step={widget.step}
        />
      );
    case "select":
      return (
        <SelectField
          key={d.field}
          {...common}
          value={typeof raw === "string" ? raw : (widget.options[0]?.value ?? "")}
          onChange={save}
          options={widget.options}
        />
      );
    case "list":
      return (
        <ListField
          key={d.field}
          {...common}
          items={Array.isArray(raw) ? (raw as string[]) : []}
          onChange={save}
          validate={listValidator(d.validation)}
        />
      );
    case "dynamic_select":
      return (
        <DynamicSelectField
          key={d.field}
          {...common}
          section={d.section}
          source={widget.source}
          dependsOn={widget.depends_on ?? []}
          sectionValues={values}
          value={str}
          onChange={save}
        />
      );
    case "cron":
      return <CronField key={d.field} {...common} value={str} onChange={save} />;
    case "object_list":
      return (
        <ObjectListField
          key={d.field}
          {...common}
          section={d.section}
          idField={widget.id_field}
          fields={widget.fields}
          minItems={widget.min_items}
          maxItems={widget.max_items}
          items={Array.isArray(raw) ? (raw as Record<string, unknown>[]) : []}
          onChange={save}
        />
      );
    case "custom": {
      const Widget = CUSTOM_SETTINGS_WIDGETS[widget.id];
      if (!Widget) {
        return <UnsupportedCustomWidget key={d.field} d={d} id={widget.id} />;
      }
      return <Widget key={d.field} descriptor={{ ...d, description: common.description }} value={raw} save={save} />;
    }
  }
}

/** Schema-driven form for one settings section; `local_only` fields are skipped and `advanced` ones folded. */
export function SchemaSection({
  section,
  schema,
  values,
  onSaveField,
  advancedSubtitle,
  onAfterSave,
  focusRequest,
  fieldAnchor,
  onlyFields,
  hideFields,
}: Props) {
  const fields = schema.filter(
    (d) =>
      d.section === section &&
      d.web_write.policy !== "local_only" &&
      (!onlyFields || onlyFields.includes(d.field)) &&
      !hideFields?.includes(d.field),
  );
  const primary = fields.filter((d) => !d.advanced);
  const advanced = fields.filter((d) => d.advanced);

  const targetField = focusRequest && focusRequest.section === section ? focusRequest.field : null;
  const targetAdvanced = !!targetField && advanced.some((d) => d.field === targetField);
  const targetRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!targetField || !targetRef.current) return;
    const el = targetRef.current;
    const reduce = window.matchMedia?.("(prefers-reduced-motion: reduce)")?.matches ?? false;
    const raf = requestAnimationFrame(() =>
      el.scrollIntoView({ block: "center", behavior: reduce ? "auto" : "smooth" }),
    );
    return () => cancelAnimationFrame(raf);
  }, [targetField]);

  const wrap = (d: SettingsFieldDescriptor, node: React.ReactNode) => {
    const isTarget = d.field === targetField;
    const anchorProps = fieldAnchor && d.field === fieldAnchor.field ? tourAnchor(fieldAnchor.anchor) : {};
    return (
      <div
        key={d.field}
        ref={isTarget ? targetRef : undefined}
        data-settings-field={`${d.section}.${d.field}`}
        className={isTarget ? "animate-settings-highlight" : undefined}
        {...anchorProps}
      >
        {node}
      </div>
    );
  };

  const makeSave =
    (d: SettingsFieldDescriptor) =>
    async (value: unknown): Promise<boolean> => {
      const result = onSaveField(d.section, d.field, value);
      const ok = result instanceof Promise ? await result : result !== false;
      // The setting is already persisted, so a failing hook must not report failure.
      if (ok && onAfterSave) {
        try {
          await onAfterSave(d, value);
        } catch (err) {
          console.warn("settings onAfterSave hook failed", err);
        }
      }
      return ok;
    };

  return (
    <div className="space-y-4">
      {primary.map((d) => wrap(d, renderField(d, values, makeSave(d))))}
      {advanced.length > 0 && (
        <CollapsibleSection title="Advanced" subtitle={advancedSubtitle} defaultOpen={targetAdvanced}>
          {advanced.map((d) => wrap(d, renderField(d, values, makeSave(d))))}
        </CollapsibleSection>
      )}
    </div>
  );
}

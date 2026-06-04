import type { SettingsFieldDescriptor, SettingsValidation } from "../../lib/types";
import {
  CollapsibleSection,
  ListField,
  NumberField,
  SelectField,
  SliderField,
  TextField,
  ToggleField,
} from "./FormFields";

interface Props {
  /** Config section name (e.g. "sandbox"). */
  section: string;
  /** Full schema descriptor list from `GET /api/settings/schema`. */
  schema: SettingsFieldDescriptor[];
  /** Current values for this section (from the effective config JSON). */
  values: Record<string, unknown>;
  /** Persist one field. Mirrors `saveField`'s (section, field, value) shape;
   *  passing `null` clears a profile override server-side. */
  onSaveField: (section: string, field: string, value: unknown) => unknown;
  /** Subtitle for the auto-generated "Advanced" fold. */
  advancedSubtitle?: string;
}

/** Client-side list-entry validator derived from the server's validation rule,
 *  purely a UX nicety; the server is authoritative either way. */
function listValidator(
  validation: SettingsValidation,
): ((value: string) => string | null) | undefined {
  if (validation.rule === "volume_list") {
    return (v) => (v.includes(":") ? null : "Must contain ':' (host:container)");
  }
  return undefined;
}

/** Render one schema-backed field with the matching FormFields control. */
function renderField(
  d: SettingsFieldDescriptor,
  values: Record<string, unknown>,
  onSaveField: Props["onSaveField"],
) {
  const raw = values[d.field];
  const save = (value: unknown) => onSaveField(d.section, d.field, value);
  const widget = d.widget;

  switch (widget.kind) {
    case "toggle":
      return (
        <ToggleField
          key={d.field}
          label={d.label}
          description={d.description}
          checked={typeof raw === "boolean" ? raw : false}
          onChange={save}
        />
      );
    case "text":
      return (
        <TextField
          key={d.field}
          label={d.label}
          description={d.description}
          value={typeof raw === "string" ? raw : ""}
          onChange={(v) => save(v)}
          mono={widget.mono}
          multiline={widget.multiline}
        />
      );
    case "optional_text":
      return (
        <TextField
          key={d.field}
          label={d.label}
          description={d.description}
          value={typeof raw === "string" ? raw : ""}
          // Empty clears the value (and the override, server-side).
          onChange={(v) => save(v || null)}
          mono={widget.mono}
        />
      );
    case "number":
      return (
        <NumberField
          key={d.field}
          label={d.label}
          description={d.description}
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
          label={d.label}
          description={d.description}
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
          label={d.label}
          description={d.description}
          value={
            typeof raw === "string"
              ? raw
              : (widget.options[0]?.value ?? "")
          }
          onChange={save}
          options={widget.options}
        />
      );
    case "list":
      return (
        <ListField
          key={d.field}
          label={d.label}
          description={d.description}
          items={Array.isArray(raw) ? (raw as string[]) : []}
          onChange={save}
          validate={listValidator(d.validation)}
        />
      );
    case "custom":
      // Bespoke widgets (theme picker, sound mode, logging matrix, ...) keep
      // their hand-written components; the generic renderer skips them.
      return null;
  }
}

/**
 * Generic schema-driven renderer for one settings section (#1692). Builds the
 * form rows from `GET /api/settings/schema` instead of hand-written per-field
 * JSX, so adding a config field surfaces here automatically. Fields the
 * dashboard may not write (`local_only`) are skipped; `advanced` fields are
 * grouped under an "Advanced" fold to match the TUI.
 */
export function SchemaSection({
  section,
  schema,
  values,
  onSaveField,
  advancedSubtitle,
}: Props) {
  const fields = schema.filter(
    (d) => d.section === section && d.web_write.policy !== "local_only",
  );
  const primary = fields.filter((d) => !d.advanced);
  const advanced = fields.filter((d) => d.advanced);

  return (
    <div className="space-y-4">
      {primary.map((d) => renderField(d, values, onSaveField))}
      {advanced.length > 0 && (
        <CollapsibleSection title="Advanced" subtitle={advancedSubtitle}>
          {advanced.map((d) => renderField(d, values, onSaveField))}
        </CollapsibleSection>
      )}
    </div>
  );
}

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
  /** Config section name (e.g. "sandbox"). */
  section: string;
  /** Full schema descriptor list from `GET /api/settings/schema`. */
  schema: SettingsFieldDescriptor[];
  /** Current values for this section (from the effective config JSON). */
  values: Record<string, unknown>;
  /** Persist one field. Mirrors `saveField`'s (section, field, value) shape;
   *  passing `null` clears a profile override server-side. May be async
   *  (returns Promise<boolean>) or sync. */
  onSaveField: (section: string, field: string, value: unknown) => unknown;
  /** Subtitle for the auto-generated "Advanced" fold. */
  advancedSubtitle?: string;
  /** Section-level post-save hook, run once after any field in this section
   *  saves successfully. Used by the acp section to refresh `serverAbout`
   *  (consumed live by ToolCards / the composer); widget-specific effects
   *  (e.g. theme repaint) live in the custom widget itself, not here. */
  onAfterSave?: (descriptor: SettingsFieldDescriptor, value: unknown) => Promise<void> | void;
  /** Set by a settings-search jump. When `section` matches this section, the
   *  named field is scrolled into view and briefly highlighted, and the
   *  Advanced fold opens if the target lives inside it. The `nonce` only
   *  participates in the SettingsView remount key; this component reacts to the
   *  fresh mount. */
  focusRequest?: { section: string; field: string; nonce: number } | null;
  /** Attach a tour anchor to one specific field's wrapper, so the first-run tour
   *  can spotlight a single control (e.g. the worktree path template) rather
   *  than the whole section. */
  fieldAnchor?: { field: string; anchor: TourAnchorId };
  /** CityHall client mode curation. `onlyFields`, when set, restricts the
   *  section to those field names (allowlist); `hideFields` drops the named
   *  fields (denylist). Both apply on top of the local_only skip. See #7. */
  onlyFields?: string[];
  hideFields?: string[];
}

/** Client-side list-entry validator derived from the server's validation rule,
 *  purely a UX nicety; the server is authoritative either way. */
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

/** Global-only fields are shown but not profile-overridable; surface that so a
 *  per-profile edit does not look profile-scoped when it is not. */
function describe(d: SettingsFieldDescriptor): string {
  if (d.profile_overridable) return d.description;
  const note = "Applies to all profiles (not profile-overridable).";
  return d.description ? `${d.description} ${note}` : note;
}

/** Visible placeholder for a `custom` widget whose `id` has no registered web
 *  component. Rendering this (rather than silently dropping the field) keeps a
 *  schema/web mismatch obvious instead of letting a setting vanish. */
function UnsupportedCustomWidget({ d, id }: { d: SettingsFieldDescriptor; id: string }) {
  return (
    <div className="text-xs text-status-error bg-status-error/10 rounded-lg p-3">
      No web control registered for "{d.label}" (custom widget "{id}"). Edit it from the TUI or <code>config.toml</code>
      .
    </div>
  );
}

/** Render one schema-backed field with the matching FormFields control. */
function renderField(
  d: SettingsFieldDescriptor,
  values: Record<string, unknown>,
  save: (value: unknown) => Promise<boolean>,
) {
  const raw = values[d.field];
  const description = describe(d);
  const widget = d.widget;

  switch (widget.kind) {
    case "toggle":
      return (
        <ToggleField
          key={d.field}
          label={d.label}
          description={description}
          checked={typeof raw === "boolean" ? raw : false}
          onChange={save}
        />
      );
    case "text":
      return (
        <TextField
          key={d.field}
          label={d.label}
          description={description}
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
          description={description}
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
          description={description}
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
          description={description}
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
          description={description}
          value={typeof raw === "string" ? raw : (widget.options[0]?.value ?? "")}
          onChange={save}
          options={widget.options}
        />
      );
    case "list":
      return (
        <ListField
          key={d.field}
          label={d.label}
          description={description}
          items={Array.isArray(raw) ? (raw as string[]) : []}
          onChange={save}
          validate={listValidator(d.validation)}
        />
      );
    case "dynamic_select":
      return (
        <DynamicSelectField
          key={d.field}
          label={d.label}
          description={description}
          section={d.section}
          source={widget.source}
          dependsOn={widget.depends_on ?? []}
          sectionValues={values}
          value={typeof raw === "string" ? raw : ""}
          onChange={save}
        />
      );
    case "cron":
      return (
        <CronField
          key={d.field}
          label={d.label}
          description={description}
          value={typeof raw === "string" ? raw : ""}
          onChange={save}
        />
      );
    case "object_list":
      return (
        <ObjectListField
          key={d.field}
          label={d.label}
          description={description}
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
      return <Widget key={d.field} descriptor={{ ...d, description }} value={raw} save={save} />;
    }
  }
}

/**
 * Generic schema-driven renderer for one settings section (#1692). Builds the
 * form rows from `GET /api/settings/schema` instead of hand-written per-field
 * JSX, so adding a config field surfaces here automatically. Fields the
 * dashboard may not write (`local_only`) are skipped; `advanced` fields are
 * grouped under an "Advanced" fold to match the TUI. `custom` widgets render
 * via the custom-widget registry (`customWidgets.tsx`).
 */
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

  // A settings-search jump targeting this section: scroll the field into view
  // (effect below) and open the Advanced fold if the field lives there. The
  // flash is a one-shot CSS animation on the target wrapper, which replays on
  // every jump because SettingsView remounts this subtree on the focus nonce.
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

  // Wrap onSaveField so a successful save in this section runs the optional
  // section-level hook once. Custom widgets receive this same `save`, so their
  // own success-gated effects compose with it.
  const makeSave =
    (d: SettingsFieldDescriptor) =>
    async (value: unknown): Promise<boolean> => {
      const result = onSaveField(d.section, d.field, value);
      const ok = result instanceof Promise ? await result : result !== false;
      // The setting is already persisted; a failing post-save hook (e.g. a
      // serverAbout refresh that errors) must not turn a successful save into
      // a failed one.
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

import { useEffect, useState } from "react";
import { fetchThemes } from "../../lib/api";
import { dispatchThemePickerChanged } from "../../hooks/useResolvedTheme";
import type { SettingsFieldDescriptor } from "../../lib/types";
import { SelectField, SliderField, TextField } from "./FormFields";

/** Props every custom settings widget receives. A custom widget renders one
 *  schema field whose `widget.kind === "custom"`; it owns any bespoke encoding
 *  (e.g. the sound `mode` enum) and any widget-specific post-save side-effect
 *  (e.g. repainting the dashboard after a theme change). Section-level effects
 *  (the acp serverAbout refresh) live in SchemaSection's `onAfterSave`, not
 *  here. */
export interface CustomWidgetProps {
  descriptor: SettingsFieldDescriptor;
  value: unknown;
  /** Persist this field. Mirrors `onSaveField` bound to (section, field).
   *  Returns the save result (a Promise<boolean> in practice) so a widget can
   *  gate a side-effect on success. */
  save: (value: unknown) => Promise<boolean> | unknown;
}

export type CustomSettingsWidget = (
  props: CustomWidgetProps,
) => React.ReactElement;

/** Resolve a save result (Promise<boolean> | unknown) to a success boolean.
 *  A non-Promise return is treated as success unless it is literally `false`,
 *  so widgets stay correct whether `onSaveField` is async or sync. */
async function didSave(result: Promise<boolean> | unknown): Promise<boolean> {
  if (result instanceof Promise) return await result;
  return result !== false;
}

/** Theme picker. Options come from the live theme list (builtins plus custom
 *  `~/.agent-of-empires/themes/*.toml`); a successful save repaints the
 *  dashboard chrome. The repaint only fires after the PATCH lands so a failed
 *  save (elevation missing, read-only, network) does not paint a theme that
 *  is not on disk (#1510). */
export function ThemeNameWidget({
  descriptor,
  value,
  save,
}: CustomWidgetProps) {
  const [themes, setThemes] = useState<string[]>([]);
  useEffect(() => {
    // Degrade to an empty list if the theme fetch fails; never leave an
    // unhandled rejection.
    fetchThemes()
      .then(setThemes)
      .catch(() => setThemes([]));
  }, []);
  return (
    <SelectField
      label={descriptor.label}
      description={descriptor.description}
      value={typeof value === "string" ? value : ""}
      onChange={async (v) => {
        if (await didSave(save(v))) {
          dispatchThemePickerChanged(v || undefined);
        }
      }}
      options={themes.map((t) => ({ value: t, label: t }))}
    />
  );
}

/** Default agent picker. The web keeps a free-text field (empty = auto-detect)
 *  rather than the TUI's agent-name select, matching prior behavior. */
export function DefaultToolWidget({
  descriptor,
  value,
  save,
}: CustomWidgetProps) {
  return (
    <TextField
      label={descriptor.label}
      description={descriptor.description}
      value={typeof value === "string" ? value : ""}
      // Empty clears the override (and falls back to auto-detect).
      onChange={(v) => save(v || null)}
      placeholder="Auto-detect"
      mono
    />
  );
}

/** Sound mode. Persisted as the string `"random"` or the tagged object
 *  `{ specific: "..." }`; this maps that enum onto a two-option select. */
export function SoundModeWidget({
  descriptor,
  value,
  save,
}: CustomWidgetProps) {
  const mode =
    typeof value === "string"
      ? value
      : typeof value === "object" && value !== null
        ? "specific"
        : "random";
  return (
    <SelectField
      label={descriptor.label}
      description={descriptor.description}
      value={mode}
      onChange={(v) => save(v === "random" ? "random" : { specific: "" })}
      options={[
        { value: "random", label: "Random" },
        { value: "specific", label: "Specific" },
      ]}
    />
  );
}

/** Playback volume. A float slider (0.1 to 1.5); the generic `slider` widget
 *  is integer-only, so this stays a custom control. */
export function SoundVolumeWidget({
  descriptor,
  value,
  save,
}: CustomWidgetProps) {
  return (
    <SliderField
      label={descriptor.label}
      description={descriptor.description}
      value={typeof value === "number" ? value : 1.0}
      onChange={save}
      min={0.1}
      max={1.5}
      step={0.1}
      formatValue={(v) => v.toFixed(1)}
    />
  );
}

function formatAcpDefaults(value: unknown): string {
  if (!value || typeof value !== "object") return "{}";
  return JSON.stringify(value, null, 2);
}

function parseAcpDefaults(value: string): Record<string, unknown> | null {
  const trimmed = value.trim();
  if (!trimmed) return {};
  try {
    const parsed = JSON.parse(trimmed);
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      return parsed as Record<string, unknown>;
    }
  } catch {
    return null;
  }
  return null;
}

/** Per-agent structured-view defaults. A map of `{ agent: { model, effort } }`,
 *  with no flat widget, so it is edited as raw JSON; an invalid edit is not
 *  saved (the field stays at its last valid value). */
export function AcpDefaultsWidget({
  descriptor,
  value,
  save,
}: CustomWidgetProps) {
  return (
    <TextField
      label={descriptor.label}
      description={descriptor.description}
      value={formatAcpDefaults(value)}
      onChange={(v) => {
        const parsed = parseAcpDefaults(v);
        if (parsed) save(parsed);
      }}
      placeholder='{"opencode":{"model":"openai/gpt-5.5","effort":"high"}}'
      mono
      multiline
    />
  );
}

// Mirrors `KNOWN_SUB_TARGETS` in src/logging.rs. Keeping this list hardcoded
// (rather than fetched) is intentional: it is the curated dropdown surface;
// advanced users can still edit `config.toml` directly or hit
// `PATCH /api/log-level` for arbitrary EnvFilter directives.
const KNOWN_TARGETS: { value: string; group: string }[] = [
  { value: "acp.protocol", group: "Structured view" },
  { value: "acp.protocol.stderr", group: "Structured view" },
  { value: "acp.protocol.tool_dispatch", group: "Structured view" },
  { value: "acp.supervisor", group: "Structured view" },
  { value: "acp.event_store", group: "Structured view" },
  { value: "acp.runner", group: "Structured view" },
  { value: "terminal.ws", group: "Terminal" },
  { value: "terminal.ws.bytes", group: "Terminal" },
  { value: "auth.token", group: "Auth" },
  { value: "auth.middleware", group: "Auth" },
  { value: "auth.rate_limit", group: "Auth" },
  { value: "auth.passphrase", group: "Auth" },
  { value: "auth.device", group: "Auth" },
  { value: "auth.ip", group: "Auth" },
  { value: "process.signal", group: "Process" },
  { value: "process.tree", group: "Process" },
  { value: "process.reap", group: "Process" },
  { value: "process.ppid", group: "Process" },
  { value: "update.fetch", group: "Update" },
  { value: "update.cache", group: "Update" },
  { value: "update.parse", group: "Update" },
  { value: "containers.docker", group: "Containers" },
  { value: "containers.image", group: "Containers" },
  { value: "containers.runtime", group: "Containers" },
  { value: "git.command", group: "Git" },
  { value: "web.client", group: "Web" },
  { value: "telemetry", group: "Telemetry" },
  { value: "http.api.telemetry", group: "Telemetry" },
  { value: "log.runtime", group: "Meta" },
];

const LEVELS = [
  { value: "", label: "(default)" },
  { value: "trace", label: "trace" },
  { value: "debug", label: "debug" },
  { value: "info", label: "info" },
  { value: "warn", label: "warn" },
  { value: "error", label: "error" },
];

/** Per-target log-level matrix. The `targets` field is a `{ target: level }`
 *  map; setting a row to "(default)" removes its override and inherits the
 *  baseline level. */
export function LoggingTargetsWidget({
  descriptor,
  value,
  save,
}: CustomWidgetProps) {
  const targets = (value ?? {}) as Record<string, string>;
  const saveTarget = (target: string, level: string) => {
    const next = { ...targets };
    if (level === "") {
      delete next[target];
    } else {
      next[target] = level;
    }
    save(next);
  };
  const grouped = KNOWN_TARGETS.reduce<Record<string, typeof KNOWN_TARGETS>>(
    (acc, t) => {
      (acc[t.group] ||= []).push(t);
      return acc;
    },
    {},
  );
  return (
    <div className="space-y-4">
      <h4 className="text-sm font-semibold text-text-primary">
        {descriptor.label}
      </h4>
      {descriptor.description && (
        <p className="text-xs text-text-dim">{descriptor.description}</p>
      )}
      {Object.entries(grouped).map(([group, items]) => (
        <div key={group} className="space-y-2">
          <h5 className="text-xs font-mono uppercase tracking-widest text-text-primary">
            {group}
          </h5>
          <div className="grid gap-3 sm:grid-cols-2">
            {items.map((t) => (
              <SelectField
                key={t.value}
                label={t.value}
                value={(targets[t.value] as string) ?? ""}
                onChange={(v) => saveTarget(t.value, v)}
                options={LEVELS}
              />
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

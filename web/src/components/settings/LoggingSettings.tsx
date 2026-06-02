import {
  CollapsibleSection,
  NumberField,
  SelectField,
  TextField,
  ToggleField,
} from "./FormFields";

// Mirrors `KNOWN_SUB_TARGETS` in src/logging.rs. Keeping this list
// hardcoded (rather than fetched) is intentional: it's the curated
// dropdown surface; advanced users can still edit `config.toml`
// directly or hit `PATCH /api/log-level` for arbitrary EnvFilter
// directives.
const KNOWN_TARGETS: { value: string; group: string }[] = [
  { value: "cockpit.acp", group: "Cockpit" },
  { value: "cockpit.acp.stderr", group: "Cockpit" },
  { value: "cockpit.acp.tool_dispatch", group: "Cockpit" },
  { value: "cockpit.supervisor", group: "Cockpit" },
  { value: "cockpit.event_store", group: "Cockpit" },
  { value: "cockpit.runner", group: "Cockpit" },
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

const DEFAULT_LEVELS = LEVELS.filter((l) => l.value !== "");

const SINK_OPTIONS = [
  { value: "file", label: "file (default)" },
  { value: "stdout", label: "stdout" },
];

const ROTATION_OPTIONS = [
  { value: "size", label: "size (default)" },
  { value: "never", label: "never" },
];

interface Props {
  settings: Record<string, unknown>;
  onSaveField: (section: string, field: string, value: unknown) => void;
  onUpdate: (patch: Record<string, unknown>) => void;
}

export function LoggingSettings({ settings, onSaveField, onUpdate }: Props) {
  const logging = (settings.logging ?? {}) as Record<string, unknown>;
  const defaultLevel = (logging.default_level as string) ?? "info";
  const targets = (logging.targets ?? {}) as Record<string, string>;
  const output = (logging.output as string) ?? "file";
  const filePath = (logging.file_path as string) ?? "debug.log";
  const rotation = (logging.rotation as string) ?? "size";
  const maxSizeMib = (logging.max_size_mib as number) ?? 50;
  const keepCount = (logging.keep_count as number) ?? 5;
  const showSpans = (logging.show_spans as boolean) ?? false;

  const saveDefaultLevel = (level: string) => {
    onUpdate({ logging: { ...logging, default_level: level } });
    onSaveField("logging", "default_level", level);
  };

  const saveTarget = (target: string, level: string) => {
    const next = { ...targets };
    if (level === "") {
      delete next[target];
    } else {
      next[target] = level;
    }
    onUpdate({ logging: { ...logging, targets: next } });
    onSaveField("logging", "targets", next);
  };

  const saveSinkField = (field: string, value: unknown) => {
    onUpdate({ logging: { ...logging, [field]: value } });
    onSaveField("logging", field, value);
  };

  // Group targets by their first segment for the UI.
  const grouped = KNOWN_TARGETS.reduce<Record<string, typeof KNOWN_TARGETS>>(
    (acc, t) => {
      (acc[t.group] ||= []).push(t);
      return acc;
    },
    {},
  );

  return (
    <div className="space-y-6">
      <div className="space-y-2">
        <p className="text-xs text-text-dim">
          Persists to <code>[logging]</code> in <code>config.toml</code>. Changes apply live to the running daemon and any cockpit subprocesses (no restart needed). The <code>AOE_LOG_LEVEL</code> env var, when set, overrides these settings at startup.
        </p>
      </div>

      <SelectField
        label="Default level"
        labelClassName="block text-sm font-semibold text-text-primary mb-1"
        description="Baseline applied to every known target root. Per-target overrides below win over this."
        value={defaultLevel}
        onChange={saveDefaultLevel}
        options={DEFAULT_LEVELS}
      />

      <div className="space-y-4">
        <h4 className="text-sm font-semibold text-text-primary">
          Per-target overrides
        </h4>
        <p className="text-xs text-text-dim">
          Each dropdown overrides the default level for a single tracing target. Set to <em>(default)</em> to remove the override and inherit the baseline.
        </p>
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

      <CollapsibleSection
        title="Advanced"
        subtitle="Sink and rotation. Some fields require restarting aoe to take effect."
      >
        <p className="text-xs text-text-dim">
          These fields change where logs land on disk and how they rotate. They are written to <code>config.toml</code> immediately but require restarting <code>aoe</code> to take effect (the tracing subscriber and rotating writer are installed once at process startup).
        </p>
        <SelectField
          label="Output"
          description="file (default) sends tracing to a log file. stdout is honored only for foreground aoe serve and env-overridden one-shot CLI; TUI / daemon child / cockpit runner coerce to file regardless."
          value={output}
          onChange={(v) => saveSinkField("output", v)}
          options={SINK_OPTIONS}
        />
        <TextField
          label="File path"
          description="Relative paths resolve under the app data dir; absolute paths are used verbatim."
          value={filePath}
          onChange={(v) => saveSinkField("file_path", v.trim() === "" ? "debug.log" : v)}
          placeholder="debug.log"
          mono
        />
        <SelectField
          label="Rotation"
          description="size rotates when the live file crosses the threshold; never disables rotation."
          value={rotation}
          onChange={(v) => saveSinkField("rotation", v)}
          options={ROTATION_OPTIONS}
        />
        <div className="grid gap-3 sm:grid-cols-2">
          <NumberField
            label="Max size (MiB)"
            description="Rotation threshold. Ignored when rotation = never."
            value={maxSizeMib}
            onChange={(v) => saveSinkField("max_size_mib", v)}
            min={1}
            max={4096}
          />
          <NumberField
            label="Keep count"
            description="How many rotated files to retain (.1 through .keep_count)."
            value={keepCount}
            onChange={(v) => saveSinkField("keep_count", v)}
            min={1}
            max={20}
          />
        </div>
        <ToggleField
          label="Show span context"
          description="When on, every log line is prefixed with the names and fields of the spans wrapping it, e.g. http_request{request_id=... method=GET path=...} from the per-request middleware. Useful for grep-correlation across async boundaries when triaging an issue, noisy on idle polling endpoints. Off by default keeps the log readable. Requires restart."
          checked={showSpans}
          onChange={(v) => saveSinkField("show_spans", v)}
        />
      </CollapsibleSection>
    </div>
  );
}

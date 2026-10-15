import { useEffect, useState } from "react";
import { fetchAgents, fetchThemes } from "../../lib/api";
import { dispatchThemePickerChanged } from "../../hooks/useResolvedTheme";
import type { AgentInfo, SettingsFieldDescriptor } from "../../lib/types";
import { SelectField, SliderField, TextField } from "./FormFields";

export interface CustomWidgetProps {
  descriptor: SettingsFieldDescriptor;
  value: unknown;
  /** Returns the save result so a widget can gate side effects on success. */
  save: (value: unknown) => Promise<boolean> | unknown;
}

export type CustomSettingsWidget = (props: CustomWidgetProps) => React.ReactElement;

async function didSave(result: Promise<boolean> | unknown): Promise<boolean> {
  if (result instanceof Promise) return await result;
  return result !== false;
}

/** Installed agents that support one-shot calls; empty if the fetch fails. */
function useOneshotAgents(): AgentInfo[] {
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  useEffect(() => {
    fetchAgents()
      .then(setAgents)
      .catch(() => setAgents([]));
  }, []);
  return agents.filter((a) => a.installed && a.oneshot_capable);
}

/** Theme picker; repaints only after the save succeeds. */
export function ThemeNameWidget({ descriptor, value, save }: CustomWidgetProps) {
  const [themes, setThemes] = useState<string[]>([]);
  useEffect(() => {
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

/** Free-text default agent; empty means auto-detect. */
export function DefaultToolWidget({ descriptor, value, save }: CustomWidgetProps) {
  return (
    <TextField
      label={descriptor.label}
      description={descriptor.description}
      value={typeof value === "string" ? value : ""}
      onChange={(v) => save(v || null)}
      placeholder="Auto-detect"
      mono
    />
  );
}

/** Agent for one-shot utility calls; empty means the session's agent. */
export function SmartRenameAgentWidget({ descriptor, value, save }: CustomWidgetProps) {
  const options = [
    { value: "", label: "Same as session" },
    ...useOneshotAgents().map((a) => ({ value: a.name, label: a.name })),
  ];
  return (
    <SelectField
      label={descriptor.label}
      description={descriptor.description}
      value={typeof value === "string" ? value : ""}
      onChange={(v) => save(v)}
      options={options}
    />
  );
}

/** Float volume slider; the generic slider is integer-only. */
export function SoundVolumeWidget({ descriptor, value, save }: CustomWidgetProps) {
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

// Mirrors `KNOWN_SUB_TARGETS` in src/logging.rs.
const KNOWN_TARGETS: { value: string; group: string }[] = [
  { value: "acp.protocol", group: "Structured view" },
  { value: "acp.protocol.stderr", group: "Structured view" },
  { value: "acp.protocol.tool_dispatch", group: "Structured view" },
  { value: "acp.supervisor", group: "Structured view" },
  { value: "acp.event_store", group: "Structured view" },
  { value: "acp.runner", group: "Structured view" },
  { value: "plugin.host", group: "Plugins" },
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

/** `{ target: level }` map; "(default)" removes the override. */
export function LoggingTargetsWidget({ descriptor, value, save }: CustomWidgetProps) {
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
  const grouped = KNOWN_TARGETS.reduce<Record<string, typeof KNOWN_TARGETS>>((acc, t) => {
    (acc[t.group] ||= []).push(t);
    return acc;
  }, {});
  return (
    <div className="space-y-4">
      <h4 className="text-sm font-semibold text-text-primary">{descriptor.label}</h4>
      {descriptor.description && <p className="text-xs text-text-dim">{descriptor.description}</p>}
      {Object.entries(grouped).map(([group, items]) => (
        <div key={group} className="space-y-2">
          <h5 className="text-xs font-mono uppercase tracking-widest text-text-primary">{group}</h5>
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

/** `{ agent: model }` map for one-shot calls; clearing a row restores the built-in default. */
export function SmartRenameModelWidget({ descriptor, value, save }: CustomWidgetProps) {
  const tunable = useOneshotAgents();
  const models = (value ?? {}) as Record<string, string>;
  const saveModel = (agent: string, model: string) => {
    const next = { ...models };
    const trimmed = model.trim();
    if (trimmed === "") {
      delete next[agent];
    } else {
      next[agent] = trimmed;
    }
    save(next);
  };
  return (
    <div className="space-y-4">
      <h4 className="text-sm font-semibold text-text-primary">{descriptor.label}</h4>
      {descriptor.description && <p className="text-xs text-text-dim">{descriptor.description}</p>}
      {tunable.length === 0 ? (
        <p className="text-xs text-text-dim">No one-shot-capable agents installed.</p>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2">
          {tunable.map((a) => (
            <TextField
              key={a.name}
              label={a.name}
              value={models[a.name] ?? ""}
              onChange={(v) => saveModel(a.name, v)}
              placeholder="Built-in default"
              mono
            />
          ))}
        </div>
      )}
    </div>
  );
}

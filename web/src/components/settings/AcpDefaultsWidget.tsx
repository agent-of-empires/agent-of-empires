// Per-agent structured-view defaults editor (#2631).
//
// Replaces the raw-JSON textarea for `session.acp_defaults` with one card per
// ACP-capable agent, each offering model / mode / thinking dropdowns plus
// per-model thinking overrides. The dropdown choices come from the recall
// catalog (`GET /api/acp/option-catalog`), which is whatever each agent last
// advertised over ACP, so new models and new agents flow in with no code
// change. When an agent has no cached options yet, or a saved value is not in
// the catalog, the control degrades to free text and flags the value as
// unverified. The raw-JSON escape hatch stays available under an advanced fold.

import { useEffect, useState } from "react";

import { fetchAcpOptionCatalog, fetchAgents } from "../../lib/api";
import type { AgentOptionEntry } from "../../lib/api";
import type { ConfigOptionCategory, ConfigOptionDescriptor } from "../../lib/acpTypes";
import type { AgentInfo } from "../../lib/types";
import type { CustomWidgetProps } from "./customWidgets";
import { SelectField, TextField } from "./FormFields";

interface AcpAgentDefaults {
  model?: string;
  effort?: string;
  mode?: string;
  effort_by_model?: Record<string, string>;
}

type DefaultsMap = Record<string, AcpAgentDefaults>;

function asMap(value: unknown): DefaultsMap {
  if (value && typeof value === "object" && !Array.isArray(value)) {
    return value as DefaultsMap;
  }
  return {};
}

function isEmptyDefaults(d: AcpAgentDefaults): boolean {
  return !d.model && !d.effort && !d.mode && (!d.effort_by_model || Object.keys(d.effort_by_model).length === 0);
}

function optionByCategory(
  options: ConfigOptionDescriptor[] | undefined,
  category: ConfigOptionCategory,
): ConfigOptionDescriptor | undefined {
  return options?.find((o) => o.category === category);
}

/** Build `<select>` options: an "adapter default" empty choice, the advertised
 *  choices, and, when the saved value is not among them, an "(unverified)"
 *  entry so a stale or hand-entered value stays selected rather than silently
 *  resetting. */
function selectOptions(
  descriptor: ConfigOptionDescriptor | undefined,
  saved: string | undefined,
  defaultLabel: string,
): { value: string; label: string }[] {
  const opts: { value: string; label: string }[] = [{ value: "", label: defaultLabel }];
  const choices = descriptor?.options ?? [];
  for (const c of choices) opts.push({ value: c.value, label: c.name || c.value });
  if (saved && !choices.some((c) => c.value === saved)) {
    opts.push({ value: saved, label: `${saved} (unverified)` });
  }
  return opts;
}

function freshness(entry: AgentOptionEntry | undefined): string {
  if (!entry) {
    return "No options cached yet. Start a structured session with this agent to populate the lists; you can type values in the meantime.";
  }
  const when = new Date(entry.updated_at);
  const stamp = isNaN(when.getTime()) ? entry.updated_at : when.toLocaleString();
  return `Options last seen ${stamp}.`;
}

/** One field: a dropdown when the agent advertised choices for the category,
 *  else a free-text input (with the same unverified-preservation intent). */
function OptionField({
  label,
  descriptor,
  value,
  onChange,
  defaultLabel,
  placeholder,
}: {
  label: string;
  descriptor: ConfigOptionDescriptor | undefined;
  value: string | undefined;
  onChange: (v: string) => void;
  defaultLabel: string;
  placeholder: string;
}) {
  if (descriptor && descriptor.options.length > 0) {
    return (
      <SelectField
        label={label}
        value={value ?? ""}
        onChange={onChange}
        options={selectOptions(descriptor, value, defaultLabel)}
      />
    );
  }
  return <TextField label={label} value={value ?? ""} onChange={onChange} placeholder={placeholder} mono />;
}

function AgentDefaultsCard({
  agent,
  entry,
  defaults,
  onChange,
}: {
  agent: AgentInfo;
  entry: AgentOptionEntry | undefined;
  defaults: AcpAgentDefaults;
  onChange: (next: AcpAgentDefaults) => void;
}) {
  const modelDesc = optionByCategory(entry?.options, "model");
  const modeDesc = optionByCategory(entry?.options, "mode");
  const effortDesc = optionByCategory(entry?.options, "thought_level");

  const set = (patch: Partial<AcpAgentDefaults>) => onChange({ ...defaults, ...patch });

  const setPerModel = (model: string, effort: string) => {
    const next = { ...(defaults.effort_by_model ?? {}) };
    if (!model || !effort) {
      if (model) delete next[model];
    } else {
      next[model] = effort;
    }
    onChange({ ...defaults, effort_by_model: next });
  };

  const removePerModel = (model: string) => {
    const next = { ...(defaults.effort_by_model ?? {}) };
    delete next[model];
    onChange({ ...defaults, effort_by_model: next });
  };

  const perModel = Object.entries(defaults.effort_by_model ?? {});
  const modelChoices = modelDesc?.options ?? [];
  const effortChoices = effortDesc?.options ?? [];
  // Models not already overridden, offered in the "add override" picker.
  const addableModels = modelChoices.filter((c) => !(c.value in (defaults.effort_by_model ?? {})));

  return (
    <div className="rounded-md border border-surface-700 bg-surface-850 p-3 space-y-3">
      <div className="flex items-baseline justify-between">
        <h5 className="text-sm font-semibold text-text-primary">{agent.name}</h5>
        {!agent.installed && <span className="text-[10px] uppercase text-text-dim">not installed</span>}
      </div>
      <p className="text-xs text-text-dim">{freshness(entry)}</p>

      <OptionField
        label="Default model"
        descriptor={modelDesc}
        value={defaults.model}
        onChange={(v) => set({ model: v || undefined })}
        defaultLabel="Adapter default"
        placeholder="e.g. openai/gpt-5.5"
      />
      <OptionField
        label="Default mode"
        descriptor={modeDesc}
        value={defaults.mode}
        onChange={(v) => set({ mode: v || undefined })}
        defaultLabel="Adapter default"
        placeholder="e.g. plan"
      />
      <OptionField
        label="Default thinking"
        descriptor={effortDesc}
        value={defaults.effort}
        onChange={(v) => set({ effort: v || undefined })}
        defaultLabel="Adapter default"
        placeholder="e.g. high"
      />

      <div className="space-y-2">
        <div className="text-sm text-text-bright">Per-model thinking</div>
        <div className="text-xs text-text-dim">
          Overrides the default thinking when that model is the resolved model.
        </div>
        {perModel.map(([model, effort]) => (
          <div key={model} className="flex items-center gap-2" data-testid={`per-model-row-${model}`}>
            <span className="flex-1 truncate font-mono text-xs text-text-secondary" title={model}>
              {model}
            </span>
            {effortChoices.length > 0 ? (
              <select
                aria-label={`Thinking for ${model}`}
                value={effort}
                onChange={(e) => setPerModel(model, e.target.value)}
                className="bg-surface-900 border border-surface-700 rounded-md px-2 py-1 text-xs text-text-primary focus:border-brand-600 focus:outline-none"
              >
                {selectOptions(effortDesc, effort, "Adapter default").map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </select>
            ) : (
              <input
                type="text"
                aria-label={`Thinking for ${model}`}
                value={effort}
                onChange={(e) => setPerModel(model, e.target.value)}
                className="w-24 bg-surface-900 border border-surface-700 rounded-md px-2 py-1 font-mono text-xs text-text-primary focus:border-brand-600 focus:outline-none"
              />
            )}
            <button
              type="button"
              aria-label={`Remove override for ${model}`}
              onClick={() => removePerModel(model)}
              className="rounded px-2 py-1 text-xs text-text-dim hover:text-text-primary"
            >
              Remove
            </button>
          </div>
        ))}
        {addableModels.length > 0 && (
          <select
            aria-label={`Add per-model thinking override for ${agent.name}`}
            value=""
            onChange={(e) => {
              const model = e.target.value;
              if (model) setPerModel(model, effortChoices[0]?.value ?? "high");
            }}
            className="bg-surface-900 border border-surface-700 rounded-md px-2 py-1 text-xs text-text-primary focus:border-brand-600 focus:outline-none"
          >
            <option value="">Add override for model…</option>
            {addableModels.map((c) => (
              <option key={c.value} value={c.value}>
                {c.name || c.value}
              </option>
            ))}
          </select>
        )}
      </div>
    </div>
  );
}

export function AcpDefaultsWidget({ descriptor, value, save }: CustomWidgetProps) {
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [catalog, setCatalog] = useState<Record<string, AgentOptionEntry>>({});
  const [rawOpen, setRawOpen] = useState(false);
  const [rawText, setRawText] = useState("");
  const [rawError, setRawError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    void (async () => {
      const [a, c] = await Promise.all([
        fetchAgents().catch(() => [] as AgentInfo[]),
        fetchAcpOptionCatalog().catch(() => ({ version: 1, agents: {} })),
      ]);
      if (!alive) return;
      setAgents(a);
      setCatalog(c.agents ?? {});
    })();
    return () => {
      alive = false;
    };
  }, []);

  const map = asMap(value);
  // ACP-capable agents drive the cards; a saved default for an agent no longer
  // in the list is still reachable through the raw-JSON fold.
  const acpAgents = agents.filter((a) => a.acp_capable);

  const saveAgent = (agentName: string, next: AcpAgentDefaults) => {
    const nextMap: DefaultsMap = { ...map };
    if (isEmptyDefaults(next)) {
      delete nextMap[agentName];
    } else {
      nextMap[agentName] = next;
    }
    void save(nextMap);
  };

  return (
    <div className="space-y-3">
      <div>
        <label className="block text-sm text-text-bright">{descriptor.label}</label>
        {descriptor.description && <div className="text-xs text-text-dim mt-0.5">{descriptor.description}</div>}
      </div>

      {acpAgents.length === 0 ? (
        <p className="text-xs text-text-dim">No ACP-capable agents detected.</p>
      ) : (
        acpAgents.map((agent) => (
          <AgentDefaultsCard
            key={agent.name}
            agent={agent}
            entry={catalog[agent.name]}
            defaults={map[agent.name] ?? {}}
            onChange={(next) => saveAgent(agent.name, next)}
          />
        ))
      )}

      <details
        open={rawOpen}
        onToggle={(e) => {
          const open = (e.target as HTMLDetailsElement).open;
          setRawOpen(open);
          if (open) {
            setRawText(JSON.stringify(map, null, 2));
            setRawError(null);
          }
        }}
      >
        <summary className="cursor-pointer text-xs text-text-dim hover:text-text-primary">
          Advanced: edit raw JSON
        </summary>
        <div className="mt-2 space-y-1">
          <TextField
            label=""
            value={rawText}
            onChange={(v) => {
              setRawText(v);
              const trimmed = v.trim();
              if (!trimmed) {
                setRawError(null);
                void save({});
                return;
              }
              try {
                const parsed = JSON.parse(trimmed);
                if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
                  setRawError(null);
                  void save(parsed);
                } else {
                  setRawError("Must be a JSON object (agent -> defaults)");
                }
              } catch {
                setRawError("Invalid JSON");
              }
            }}
            placeholder='{"opencode":{"model":"openai/gpt-5.5","effort":"high"}}'
            mono
            multiline
          />
          {rawError && <div className="text-xs text-red-400">{rawError}</div>}
        </div>
      </details>
    </div>
  );
}

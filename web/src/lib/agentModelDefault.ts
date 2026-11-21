import { fetchSettings, getProfileSettings, updateSettings, type SaveOutcome, type ServerAbout } from "./api";

/** Where a Session's model picker can save its Agent's default; null where Settings can't be written. */
export interface AgentDefaultsTarget {
  profile: string;
  /** Opens Settings › Structured view at the Agent defaults row. */
  open: () => void;
}

/** The Settings row for the profile's Agent defaults, or null on read-only, CityHall, or before /api/about answers. */
export function agentDefaultsHref(profile: string, about: ServerAbout | null | undefined): string | null {
  if (!about || about.read_only || about.cityhall_mode) return null;
  return `/settings/structured-view?profile=${encodeURIComponent(profile)}&field=acp.acp_defaults`;
}

export interface AgentModelDefault {
  /** The configured default model; null when the Agent picks its own. */
  model: string | null;
  pinned: boolean;
}

type Entry = Record<string, unknown>;
type DefaultsMap = Record<string, Entry>;

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const text = (v: unknown) => (typeof v === "string" && v.trim() ? v : null);

function defaultsMap(settings: unknown): DefaultsMap {
  const acp = isObject(settings) ? settings.acp : undefined;
  const map = isObject(acp) ? acp.acp_defaults : undefined;
  return isObject(map) ? (map as DefaultsMap) : {};
}

// Mirrors `AcpAgentDefaults::is_empty`.
function isEmpty(entry: Entry | undefined): boolean {
  if (!entry) return true;
  const byModel = entry.effort_by_model;
  return (
    !text(entry.model) &&
    entry.pin_model !== true &&
    !text(entry.effort) &&
    !text(entry.mode) &&
    !(isObject(byModel) && Object.keys(byModel).length > 0)
  );
}

/** The key `acp_defaults_for` reads for `agent`: its own entry, else `claude` for the legacy `claude-code`. */
function entryKey(map: DefaultsMap, agent: string): string {
  if (!isEmpty(map[agent])) return agent;
  if (agent === "claude-code" && !isEmpty(map.claude)) return "claude";
  return agent;
}

/** `agent`'s default model in resolved settings, as the Daemon reads it at spawn. */
export function agentModelDefault(resolved: unknown, agent: string): AgentModelDefault {
  const map = defaultsMap(resolved);
  const entry = map[entryKey(map, agent)];
  const model = isEmpty(entry) ? null : text(entry!.model);
  return { model, pinned: model !== null && entry!.pin_model === true };
}

export async function loadAgentModelDefault(profile: string, agent: string): Promise<AgentModelDefault | null> {
  const resolved = await fetchSettings(profile);
  return resolved ? agentModelDefault(resolved, agent) : null;
}

/** The profile's own `acp_defaults` with only `model` changed in the entry the Daemon reads for `agent`. */
export function withDefaultModel(override: unknown, resolved: unknown, agent: string, model: string): DefaultsMap {
  const map = defaultsMap(override);
  const key = entryKey(defaultsMap(resolved), agent);
  return { ...map, [key]: { ...map[key], model } };
}

/**
 * Saves `model` as `agent`'s default for `profile`. A save replaces the profile's whole
 * `acp_defaults`, so it writes the profile's own entries back with one field changed: fields
 * the profile inherits stay inherited. Nothing is written when either read fails.
 */
export async function saveAgentModelDefault(profile: string, agent: string, model: string): Promise<SaveOutcome> {
  const [override, resolved] = await Promise.all([getProfileSettings(profile), fetchSettings(profile)]);
  if (!override || !resolved) return "rejected";
  return updateSettings({ acp: { acp_defaults: withDefaultModel(override, resolved, agent, model) } }, profile);
}

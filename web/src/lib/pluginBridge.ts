// Plugin page bridge v1: validation and gating for messages a page frame posts to the dashboard. See docs/plugin-api.md#page-bridge.

import type { ResolvedTheme } from "./theme";

export const BRIDGE_VERSION = 1;

export type BridgeErrorCode =
  | "unknown_method"
  | "invalid_params"
  | "capability_denied"
  | "read_only"
  | "cityhall_mode"
  | "failed";

export type BridgeId = string | number;

export interface BridgeRequest {
  id: BridgeId;
  method: string;
  params: Record<string, unknown>;
}

export type BridgeCall =
  | { method: "ready" }
  | { method: "sessions.list" }
  | { method: "plugins.uiState" }
  | { method: "action"; action: string; params: Record<string, unknown>; sessionId?: string }
  | { method: "openSession"; sessionId: string }
  | { method: "session.archive" | "session.trash"; sessionId: string };

export interface BridgeGrants {
  capabilities: readonly string[];
  readOnly: boolean;
  cityhall: boolean;
}

export type BridgeResolution = { call: BridgeCall } | { error: BridgeErrorCode; message: string };

const isRecord = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);

const isText = (v: unknown): v is string => typeof v === "string" && v.length > 0;

/** The envelope, or null when the message must be dropped without a reply. */
export function parseRequest(data: unknown): BridgeRequest | null {
  if (!isRecord(data) || data.aoeBridge !== BRIDGE_VERSION) return null;
  const { id, method, params } = data;
  const validId = (isText(id) && id.length <= 128) || (typeof id === "number" && Number.isFinite(id));
  if (!validId || !isText(method)) return null;
  if (params !== undefined && !isRecord(params)) return null;
  return { id, method, params: params ?? {} };
}

type Parse = (params: Record<string, unknown>) => BridgeCall | null;

const sessionTarget =
  (method: "session.archive" | "session.trash"): Parse =>
  ({ id }) =>
    isText(id) ? { method, sessionId: id } : null;

const METHODS = new Map<string, { capability?: string; mutates?: boolean; parse: Parse }>([
  ["ready", { parse: () => ({ method: "ready" }) }],
  ["sessions.list", { capability: "session.read", parse: () => ({ method: "sessions.list" }) }],
  ["plugins.uiState", { capability: "session.read", parse: () => ({ method: "plugins.uiState" }) }],
  [
    "action",
    {
      mutates: true,
      parse: ({ method, params, sessionId }) => {
        if (!isText(method) || (params !== undefined && !isRecord(params))) return null;
        if (sessionId !== undefined && !isText(sessionId)) return null;
        return { method: "action", action: method, params: params ?? {}, sessionId };
      },
    },
  ],
  ["openSession", { parse: ({ id }) => (isText(id) ? { method: "openSession", sessionId: id } : null) }],
  ["session.archive", { capability: "session.write", mutates: true, parse: sessionTarget("session.archive") }],
  ["session.trash", { capability: "session.write", mutates: true, parse: sessionTarget("session.trash") }],
]);

/** Validates params, then gates on the plugin's capabilities and the dashboard mode. */
export function resolveRequest(req: BridgeRequest, grants: BridgeGrants): BridgeResolution {
  const spec = METHODS.get(req.method);
  if (!spec) return { error: "unknown_method", message: `Unknown method ${req.method}.` };
  const call = spec.parse(req.params);
  if (!call) return { error: "invalid_params", message: `Invalid params for ${req.method}.` };
  if (spec.capability && !grants.capabilities.includes(spec.capability)) {
    return { error: "capability_denied", message: `${req.method} requires the ${spec.capability} capability.` };
  }
  if (spec.mutates && grants.readOnly) return { error: "read_only", message: "The dashboard is read-only." };
  if (spec.mutates && grants.cityhall) return { error: "cityhall_mode", message: "Not available in CityHall mode." };
  return { call };
}

export const bridgeReply = (id: BridgeId, result: unknown) => ({ aoeBridge: BRIDGE_VERSION, id, result });

export const bridgeError = (id: BridgeId, code: BridgeErrorCode, message: string) => ({
  aoeBridge: BRIDGE_VERSION,
  id,
  error: { code, message },
});

export const bridgePush = (event: "context" | "theme", data: unknown) => ({ aoeBridge: BRIDGE_VERSION, event, data });

export const themePayload = (theme: ResolvedTheme) => ({
  appearance: theme.appearance,
  cssVars: { ...theme.web.cssVars, ...theme.terminal.cssVars },
});

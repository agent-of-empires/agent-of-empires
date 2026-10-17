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
  | { method: "session.archive" | "session.trash"; sessionId: string }
  | { method: "session.contextMenu"; sessionId: string; x: number; y: number; items: PageMenuItem[] };

/** A page-supplied entry appended to the host session menu; picking it resolves the call with its id. */
export interface PageMenuItem {
  id: string;
  label: string;
}

const MAX_MENU_ITEMS = 20;

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

// The target must be a session sessions.list returns, so the page needs to read it too.
const TARGET_CAPS = ["session.read", "session.write"];

const isCoord = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v) && v >= 0;

const isMenuItem = (v: unknown): v is PageMenuItem =>
  isRecord(v) && isText(v.id) && v.id.length <= 128 && isText(v.label) && v.label.length <= 80;

const parseContextMenu: Parse = ({ id, x, y, items }) => {
  if (!isText(id) || !isCoord(x) || !isCoord(y)) return null;
  if (items !== undefined && !(Array.isArray(items) && items.length <= MAX_MENU_ITEMS && items.every(isMenuItem))) {
    return null;
  }
  return { method: "session.contextMenu", sessionId: id, x, y, items: items ?? [] };
};

const sessionTarget =
  (method: "session.archive" | "session.trash"): Parse =>
  ({ id }) =>
    isText(id) ? { method, sessionId: id } : null;

const METHODS = new Map<string, { capabilities?: string[]; mutates?: boolean; parse: Parse }>([
  ["ready", { parse: () => ({ method: "ready" }) }],
  ["sessions.list", { capabilities: ["session.read"], parse: () => ({ method: "sessions.list" }) }],
  ["plugins.uiState", { capabilities: ["session.read"], parse: () => ({ method: "plugins.uiState" }) }],
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
  ["session.archive", { capabilities: TARGET_CAPS, mutates: true, parse: sessionTarget("session.archive") }],
  ["session.trash", { capabilities: TARGET_CAPS, mutates: true, parse: sessionTarget("session.trash") }],
  // Not `mutates`: the user picks every action in the host's own menu, which applies the dashboard's modes itself.
  ["session.contextMenu", { capabilities: ["session.read"], parse: parseContextMenu }],
]);

/** Validates params, then gates on the plugin's capabilities and the dashboard mode. */
export function resolveRequest(req: BridgeRequest, grants: BridgeGrants): BridgeResolution {
  const spec = METHODS.get(req.method);
  if (!spec) return { error: "unknown_method", message: `Unknown method ${req.method}.` };
  const call = spec.parse(req.params);
  if (!call) return { error: "invalid_params", message: `Invalid params for ${req.method}.` };
  const missing = spec.capabilities?.find((cap) => !grants.capabilities.includes(cap));
  if (missing) return { error: "capability_denied", message: `${req.method} requires the ${missing} capability.` };
  if (spec.mutates && grants.readOnly) return { error: "read_only", message: "The dashboard is read-only." };
  if (spec.mutates && grants.cityhall) return { error: "cityhall_mode", message: "Not available in CityHall mode." };
  return { call };
}

/** Thrown by a call to reply with `code` rather than `failed`. */
export class BridgeRefusal extends Error {
  readonly code: BridgeErrorCode;
  constructor(code: BridgeErrorCode, message: string) {
    super(message);
    this.code = code;
  }
}

export const bridgeReply = (id: BridgeId, result: unknown) => ({ aoeBridge: BRIDGE_VERSION, id, result });

export const bridgeError = (id: BridgeId, code: BridgeErrorCode, message: string) => ({
  aoeBridge: BRIDGE_VERSION,
  id,
  error: { code, message },
});

export const bridgePush = (event: "context" | "theme" | "selection", data: unknown) => ({
  aoeBridge: BRIDGE_VERSION,
  event,
  data,
});

export const themePayload = (theme: ResolvedTheme) => ({
  appearance: theme.appearance,
  cssVars: { ...theme.web.cssVars, ...theme.terminal.cssVars },
});

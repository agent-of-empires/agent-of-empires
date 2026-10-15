import { describe, expect, it } from "vitest";
import { parseRequest, resolveRequest, type BridgeGrants } from "../pluginBridge";

const open: BridgeGrants = { capabilities: ["session.read"], readOnly: false, cityhall: false };

describe("parseRequest", () => {
  it.each([
    ["a non-object", "ready"],
    ["null", null],
    ["a missing version", { id: 1, method: "ready" }],
    ["another version", { aoeBridge: 2, id: 1, method: "ready" }],
    ["a missing id", { aoeBridge: 1, method: "ready" }],
    ["an empty id", { aoeBridge: 1, id: "", method: "ready" }],
    ["an oversized id", { aoeBridge: 1, id: "x".repeat(129), method: "ready" }],
    ["a non-finite id", { aoeBridge: 1, id: Number.NaN, method: "ready" }],
    ["a missing method", { aoeBridge: 1, id: 1 }],
    ["array params", { aoeBridge: 1, id: 1, method: "ready", params: [] }],
  ])("drops %s", (_name, data) => {
    expect(parseRequest(data)).toBeNull();
  });

  it("accepts string and number ids and defaults params", () => {
    expect(parseRequest({ aoeBridge: 1, id: "a", method: "ready" })).toEqual({ id: "a", method: "ready", params: {} });
    expect(parseRequest({ aoeBridge: 1, id: 7, method: "openSession", params: { id: "s" } })).toEqual({
      id: 7,
      method: "openSession",
      params: { id: "s" },
    });
  });
});

describe("resolveRequest", () => {
  const resolve = (method: string, params: Record<string, unknown>, grants: Partial<BridgeGrants> = {}) =>
    resolveRequest({ id: 1, method, params }, { ...open, ...grants });
  const errorOf = (r: ReturnType<typeof resolve>) => ("error" in r ? r.error : null);

  it.each([
    { method: "ready", params: {}, grants: { capabilities: [] }, error: null },
    { method: "sessions.list", params: {}, grants: {}, error: null },
    { method: "sessions.list", params: {}, grants: { capabilities: [] }, error: "capability_denied" },
    { method: "plugins.uiState", params: {}, grants: { capabilities: ["session.write"] }, error: "capability_denied" },
    { method: "plugins.uiState", params: {}, grants: { readOnly: true, cityhall: true }, error: null },
    { method: "action", params: { method: "move" }, grants: { capabilities: [] }, error: null },
    { method: "action", params: { method: "move" }, grants: { readOnly: true }, error: "read_only" },
    { method: "action", params: { method: "move" }, grants: { cityhall: true }, error: "cityhall_mode" },
    { method: "action", params: {}, grants: {}, error: "invalid_params" },
    { method: "action", params: { method: "move", params: "x" }, grants: {}, error: "invalid_params" },
    { method: "action", params: { method: "move", sessionId: 3 }, grants: {}, error: "invalid_params" },
    { method: "openSession", params: { id: "s" }, grants: { readOnly: true, cityhall: true }, error: null },
    { method: "openSession", params: {}, grants: {}, error: "invalid_params" },
    { method: "session.archive", params: { id: "s" }, grants: { capabilities: ["session.write"] }, error: null },
    { method: "session.trash", params: { id: "s" }, grants: { capabilities: ["session.write"] }, error: null },
    { method: "session.archive", params: { id: "s" }, grants: {}, error: "capability_denied" },
    { method: "session.trash", params: { id: "s" }, grants: {}, error: "capability_denied" },
    { method: "session.trash", params: {}, grants: { capabilities: ["session.write"] }, error: "invalid_params" },
    {
      method: "session.archive",
      params: { id: "s" },
      grants: { capabilities: ["session.write"], readOnly: true },
      error: "read_only",
    },
    {
      method: "session.trash",
      params: { id: "s" },
      grants: { capabilities: ["session.write"], cityhall: true },
      error: "cityhall_mode",
    },
    { method: "sessions.archive", params: {}, grants: {}, error: "unknown_method" },
    { method: "toString", params: {}, grants: {}, error: "unknown_method" },
  ])("$method with $grants -> $error", ({ method, params, grants, error }) => {
    expect(errorOf(resolve(method, params, grants))).toBe(error);
  });

  it("maps a session target to the call", () => {
    expect(resolve("session.trash", { id: "s" }, { capabilities: ["session.write"] })).toEqual({
      call: { method: "session.trash", sessionId: "s" },
    });
  });

  it("maps action params to the call", () => {
    expect(resolve("action", { method: "move", params: { to: "done" }, sessionId: "s" })).toEqual({
      call: { method: "action", action: "move", params: { to: "done" }, sessionId: "s" },
    });
  });
});

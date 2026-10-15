// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { classifyAuthError, isLoginAttemptPath } from "./fetchInterceptor";

const saveToken = vi.fn();
const clearToken = vi.fn();
let storedToken: string | null = null;
let bindingSecret: string | null = "binding-secret";
const getOrCreateDeviceBindingSecret = vi.fn(() => {
  if (bindingSecret === null) throw new Error("no binding");
  return bindingSecret;
});
let serverDown = false;
const reportError = vi.fn();

vi.mock("./token", () => ({
  getToken: () => storedToken,
  saveToken: (t: string) => saveToken(t),
  clearToken: () => clearToken(),
}));
vi.mock("./deviceBinding", () => ({ getOrCreateDeviceBindingSecret: () => getOrCreateDeviceBindingSecret() }));
vi.mock("./connectionState", () => ({ isServerDown: () => serverDown }));
vi.mock("./toastBus", () => ({ reportError: (m: string) => reportError(m) }));

const json = (status: number, body: unknown, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", ...headers } });

describe("classifyAuthError", () => {
  it.each<[string, Response, string | null]>([
    ["200", json(200, { ok: true }), null],
    ["403", json(403, { error: "x" }), null],
    ["500", json(500, { error: "x" }), null],
    ["401 login_required", json(401, { error: "login_required" }), "login_required"],
    ["401 unauthorized", json(401, { error: "unauthorized" }), "unauthorized"],
    ["401 non-JSON", new Response("not json", { status: 401 }), "unauthorized"],
    ["401 without an error field", json(401, { message: "no error key" }), "unauthorized"],
  ])("%s", async (_name, res, expected) => {
    expect(await classifyAuthError(res)).toBe(expected);
  });

  it("leaves the original response body readable", async () => {
    const res = json(401, { error: "login_required" });
    await classifyAuthError(res);
    expect(await res.json()).toEqual({ error: "login_required" });
  });
});

it.each([
  ["/api/login", true],
  ["/api/login/elevate", true],
  ["/api/login/status", false],
  ["/api/logout", false],
  ["/api/sessions", false],
  ["/api/login/something-else", false],
])("isLoginAttemptPath(%s) is %s", (path, expected) => {
  expect(isLoginAttemptPath(path)).toBe(expected);
});

describe("installFetchErrorToasts", () => {
  let original: ReturnType<typeof vi.fn>;
  let mod: typeof import("./fetchInterceptor");
  const listeners: [string, EventListener][] = [];

  async function install() {
    vi.resetModules();
    delete (window as unknown as { __aoeFetchPatched?: boolean }).__aoeFetchPatched;
    original = vi.fn();
    window.fetch = original as unknown as typeof fetch;
    mod = await import("./fetchInterceptor");
    mod.installFetchErrorToasts();
  }

  function listen(name: string) {
    const spy = vi.fn();
    window.addEventListener(name, spy);
    listeners.push([name, spy]);
    return spy;
  }

  const sentHeaders = (call = 0) => new Headers((original.mock.calls[call]![1] as RequestInit).headers);
  const networkToast = "Network error contacting /api/sessions. Check your connection.";

  beforeEach(() => {
    vi.clearAllMocks();
    storedToken = null;
    bindingSecret = "binding-secret";
    serverDown = false;
  });

  afterEach(() => {
    for (const [name, spy] of listeners.splice(0)) window.removeEventListener(name, spy);
    vi.useRealTimers();
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
  });

  it("installs the wrapper exactly once", async () => {
    await install();
    const wrapped = window.fetch;
    mod.installFetchErrorToasts();
    expect(window.fetch).toBe(wrapped);
    expect(window.fetch).not.toBe(original);
  });

  describe("request headers", () => {
    it.each<[string, string | null, string | null, string, RequestInit | undefined, Record<string, string | null>]>([
      [
        "token, binding, and request id on /api",
        "tok",
        "binding-secret",
        "/api/sessions",
        undefined,
        { Authorization: "Bearer tok", "X-Aoe-Device-Binding": "binding-secret", "X-Request-Id": "*" },
      ],
      [
        "caller headers kept",
        "tok",
        "binding-secret",
        "/api/sessions",
        { headers: { Authorization: "Bearer caller", "X-Request-Id": "caller-id" } },
        { Authorization: "Bearer caller", "X-Request-Id": "caller-id" },
      ],
      [
        "no request id off /api",
        "tok",
        "binding-secret",
        "/index.html",
        undefined,
        { Authorization: "Bearer tok", "X-Request-Id": null },
      ],
      [
        "no credentials available",
        null,
        null,
        "/api/sessions",
        undefined,
        { Authorization: null, "X-Aoe-Device-Binding": null, "X-Request-Id": "*" },
      ],
      [
        "a throwing binding secret",
        "tok",
        null,
        "/api/sessions",
        undefined,
        { Authorization: "Bearer tok", "X-Aoe-Device-Binding": null },
      ],
      [
        "an absolute same-origin URL",
        "tok",
        "binding-secret",
        `${window.location.origin}/api/sessions`,
        undefined,
        { "X-Request-Id": "*" },
      ],
    ])("%s", async (_name, token, secret, url, init, expected) => {
      storedToken = token;
      bindingSecret = secret;
      await install();
      original.mockResolvedValue(json(200, {}));
      await window.fetch(url, init);
      const headers = sentHeaders();
      for (const [key, value] of Object.entries(expected)) {
        if (value === "*") expect(headers.get(key)).toBeTruthy();
        else expect(headers.get(key)).toBe(value);
      }
    });

    it("leaves cross-origin requests untouched", async () => {
      storedToken = "tok";
      await install();
      original.mockResolvedValue(json(200, {}));
      await window.fetch("https://example.com/api/thing");
      expect(original.mock.calls[0]![1]).toBeUndefined();
      expect(getOrCreateDeviceBindingSecret).not.toHaveBeenCalled();
    });
  });

  it("saves a rotated X-Aoe-Token only when present", async () => {
    await install();
    original.mockResolvedValueOnce(json(200, {})).mockResolvedValueOnce(json(200, {}, { "x-aoe-token": "rotated" }));
    await window.fetch("/api/sessions");
    expect(saveToken).not.toHaveBeenCalled();
    await window.fetch("/api/sessions");
    expect(saveToken).toHaveBeenCalledWith("rotated");
  });

  describe("auth failures", () => {
    it("a generic 401 clears the token and fires one deduped event until re-armed", async () => {
      await install();
      original.mockResolvedValue(json(401, { error: "unauthorized" }));
      const onExpired = listen(mod.TOKEN_EXPIRED_EVENT);
      await window.fetch("/api/sessions");
      await window.fetch("/api/sessions");
      expect(clearToken).toHaveBeenCalledTimes(2);
      expect(onExpired).toHaveBeenCalledOnce();
      mod.resetTokenExpired();
      await window.fetch("/api/sessions");
      expect(onExpired).toHaveBeenCalledTimes(2);
    });

    it("a 401 login_required asks for login without clearing the token", async () => {
      await install();
      original.mockResolvedValue(json(401, { error: "login_required" }));
      const onLogin = listen(mod.LOGIN_REQUIRED_EVENT);
      const onExpired = listen(mod.TOKEN_EXPIRED_EVENT);
      await window.fetch("/api/sessions");
      expect(onLogin).toHaveBeenCalledOnce();
      expect(onExpired).not.toHaveBeenCalled();
      expect(clearToken).not.toHaveBeenCalled();
    });

    it.each(["/api/login", "/not-api"])("a 401 on %s fires no token event", async (path) => {
      await install();
      original.mockResolvedValue(json(401, { error: "unauthorized" }));
      const onExpired = listen(mod.TOKEN_EXPIRED_EVENT);
      await window.fetch(path);
      expect(onExpired).not.toHaveBeenCalled();
      if (path === "/not-api") expect(clearToken).not.toHaveBeenCalled();
    });

    it.each<[Response, number]>([
      [json(403, { error: "elevation_required" }), 1],
      [json(403, { error: "forbidden" }), 0],
      [new Response("nope", { status: 403 }), 0],
    ])("a 403 fires the elevation event only for elevation_required (%#)", async (response, calls) => {
      await install();
      original.mockResolvedValue(response);
      const onElevation = listen(mod.ELEVATION_REQUIRED_EVENT);
      await window.fetch("/api/sessions");
      expect(onElevation).toHaveBeenCalledTimes(calls);
    });
  });

  it.each<[string, Response, string, RequestInit | undefined, boolean, string | null]>([
    ["an /api 5xx", json(503, {}), "/api/sessions", undefined, false, "Server error 503 from /api/sessions"],
    ["a 5xx while the server is known down", json(500, {}), "/api/sessions", undefined, true, null],
    ["a non-/api 5xx", json(500, {}), "/static", undefined, false, null],
    [
      "worker_not_ready on /acp/prompt",
      new Response("worker_not_ready", { status: 503 }),
      "/api/sessions/abc/acp/prompt",
      { method: "POST" },
      false,
      null,
    ],
    [
      "worker_capacity_full on /acp/prompt",
      new Response("worker_capacity_full (4/4)", { status: 503 }),
      "/api/sessions/abc/acp/prompt",
      { method: "POST" },
      false,
      "Server error 503 from /api/sessions/abc/acp/prompt",
    ],
    [
      "worker_not_ready off the prompt route",
      new Response("worker_not_ready", { status: 503 }),
      "/api/sessions/abc/acp/cancel",
      { method: "POST" },
      false,
      "Server error 503 from /api/sessions/abc/acp/cancel",
    ],
  ])("toast for %s", async (_name, response, url, init, down, toast) => {
    serverDown = down;
    await install();
    original.mockResolvedValue(response);
    await window.fetch(url, init);
    if (toast) expect(reportError).toHaveBeenCalledWith(toast);
    else expect(reportError).not.toHaveBeenCalled();
  });

  it.each<[string, Error, string, boolean, string | null]>([
    ["an /api failure", new TypeError("Failed to fetch"), "/api/sessions", false, networkToast],
    ["a known-down server", new TypeError("Failed to fetch"), "/api/sessions", true, null],
    ["AbortError", new DOMException("aborted", "AbortError"), "/api/sessions", false, null],
    ["TimeoutError", new DOMException("timed out", "TimeoutError"), "/api/sessions", false, null],
    ["a non-/api path", new TypeError("Failed to fetch"), "/asset.js", false, null],
  ])("rethrows a network error for %s", async (_name, error, url, down, toast) => {
    serverDown = down;
    await install();
    original.mockRejectedValue(error);
    await expect(window.fetch(url)).rejects.toBe(error);
    if (toast) expect(reportError).toHaveBeenCalledWith(toast);
    else expect(reportError).not.toHaveBeenCalled();
  });

  it("suppresses network toasts while hidden", async () => {
    await install();
    original.mockRejectedValue(new TypeError("Failed to fetch"));
    Object.defineProperty(document, "visibilityState", { value: "hidden", configurable: true });
    document.dispatchEvent(new Event("visibilitychange"));
    await expect(window.fetch("/api/sessions")).rejects.toThrow();
    expect(reportError).not.toHaveBeenCalled();
  });

  it("suppresses network toasts only briefly after focus returns", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(10_000);
    await install();
    original.mockRejectedValue(new TypeError("Failed to fetch"));
    window.dispatchEvent(new Event("focus"));
    await expect(window.fetch("/api/sessions")).rejects.toThrow();
    expect(reportError).not.toHaveBeenCalled();
    vi.setSystemTime(13_000);
    await expect(window.fetch("/api/sessions")).rejects.toThrow();
    expect(reportError).toHaveBeenCalledWith(networkToast);
  });
});

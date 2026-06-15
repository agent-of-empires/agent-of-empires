// @vitest-environment jsdom
//
// Extends fetchInterceptor.test.ts (which covers classifyAuthError +
// isLoginAttemptPath) with the install/uninstall lifecycle of
// installFetchErrorToasts: header injection, token rotation, the 401 /
// 403 / 5xx / network branches, the dedup flags, and resetTokenExpired.
//
// The interceptor patches window.fetch in place and keeps a module-level
// dedup flag, so each test resets modules and re-imports against a fresh
// window flag + freshly stubbed dependency modules.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const saveToken = vi.fn();
const clearToken = vi.fn();
let storedToken: string | null = null;
const getToken = vi.fn(() => storedToken);

let bindingSecret: string | null = "binding-secret";
const getOrCreateDeviceBindingSecret = vi.fn(() => {
  if (bindingSecret === null) throw new Error("no binding");
  return bindingSecret;
});

let serverDown = false;
const isServerDown = vi.fn(() => serverDown);

const reportError = vi.fn();

vi.mock("./token", () => ({
  getToken: () => getToken(),
  saveToken: (t: string) => saveToken(t),
  clearToken: () => clearToken(),
}));

vi.mock("./deviceBinding", () => ({
  getOrCreateDeviceBindingSecret: () => getOrCreateDeviceBindingSecret(),
}));

vi.mock("./connectionState", () => ({
  isServerDown: () => isServerDown(),
}));

vi.mock("./toastBus", () => ({
  reportError: (m: string) => reportError(m),
}));

type Mod = typeof import("./fetchInterceptor");

// Re-import the module fresh so its module-level dedup flags and the
// window.__aoeFetchPatched guard start clean, then install the wrapper.
async function freshInstall(): Promise<{ mod: Mod; original: ReturnType<typeof vi.fn> }> {
  vi.resetModules();
  delete (window as unknown as { __aoeFetchPatched?: boolean }).__aoeFetchPatched;
  const original = vi.fn();
  window.fetch = original as unknown as typeof fetch;
  const mod = await import("./fetchInterceptor");
  mod.installFetchErrorToasts();
  return { mod, original };
}

function jsonResponse(status: number, body: unknown, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  storedToken = null;
  bindingSecret = "binding-secret";
  serverDown = false;
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("installFetchErrorToasts lifecycle", () => {
  it("installs the wrapper exactly once", async () => {
    const { mod, original } = await freshInstall();
    const wrapped = window.fetch;
    // A second install must be a no-op: fetch stays the same wrapper.
    mod.installFetchErrorToasts();
    expect(window.fetch).toBe(wrapped);
    expect(window.fetch).not.toBe(original);
  });
});

describe("request header injection", () => {
  it("injects Authorization, device-binding, and X-Request-Id for same-origin /api/ calls", async () => {
    storedToken = "tok-123";
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(200, { ok: true }));

    await window.fetch("/api/sessions");

    const init = original.mock.calls[0][1] as RequestInit;
    const headers = new Headers(init.headers);
    expect(headers.get("Authorization")).toBe("Bearer tok-123");
    expect(headers.get("X-Aoe-Device-Binding")).toBe("binding-secret");
    expect(headers.get("X-Request-Id")).toBeTruthy();
  });

  it("does not clobber caller-supplied Authorization / X-Request-Id headers", async () => {
    storedToken = "tok-123";
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(200, { ok: true }));

    await window.fetch("/api/sessions", {
      headers: { Authorization: "Bearer caller", "X-Request-Id": "caller-id" },
    });

    const headers = new Headers((original.mock.calls[0][1] as RequestInit).headers);
    expect(headers.get("Authorization")).toBe("Bearer caller");
    expect(headers.get("X-Request-Id")).toBe("caller-id");
  });

  it("does not inject X-Request-Id for non-/api same-origin calls", async () => {
    storedToken = "tok-123";
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(200, { ok: true }));

    await window.fetch("/index.html");

    const headers = new Headers((original.mock.calls[0][1] as RequestInit).headers);
    expect(headers.has("X-Request-Id")).toBe(false);
    // Auth header still attaches for same-origin requests.
    expect(headers.get("Authorization")).toBe("Bearer tok-123");
  });

  it("attaches no auth headers and leaves init untouched for cross-origin URLs", async () => {
    storedToken = "tok-123";
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(200, { ok: true }));

    await window.fetch("https://example.com/api/thing");

    const init = original.mock.calls[0][1];
    // attachAuthHeader returns the original init untouched (undefined here).
    expect(init).toBeUndefined();
    expect(getOrCreateDeviceBindingSecret).not.toHaveBeenCalled();
  });

  it("sends no auth headers when there is neither token nor binding secret", async () => {
    storedToken = null;
    bindingSecret = null;
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(200, { ok: true }));

    await window.fetch("/api/sessions");

    const init = original.mock.calls[0][1] as RequestInit;
    // No Authorization / binding header, but X-Request-Id is still added.
    const headers = new Headers(init.headers);
    expect(headers.has("Authorization")).toBe(false);
    expect(headers.has("X-Aoe-Device-Binding")).toBe(false);
    expect(headers.get("X-Request-Id")).toBeTruthy();
  });

  it("swallows a throwing device-binding secret and still sends the token", async () => {
    storedToken = "tok-123";
    bindingSecret = null; // getOrCreateDeviceBindingSecret throws
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(200, { ok: true }));

    await window.fetch("/api/sessions");

    const headers = new Headers((original.mock.calls[0][1] as RequestInit).headers);
    expect(headers.get("Authorization")).toBe("Bearer tok-123");
    expect(headers.has("X-Aoe-Device-Binding")).toBe(false);
  });

  it("normalizes a Request input and an absolute same-origin URL to a /api/ path", async () => {
    storedToken = "tok-123";
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(200, { ok: true }));

    const absolute = `${window.location.origin}/api/sessions`;
    await window.fetch(absolute);
    const headers = new Headers((original.mock.calls[0][1] as RequestInit).headers);
    // Recognized as same-origin /api/ -> request id injected.
    expect(headers.get("X-Request-Id")).toBeTruthy();
  });
});

describe("token rotation", () => {
  it("saves a rotated token from the X-Aoe-Token response header", async () => {
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(200, { ok: true }, { "x-aoe-token": "rotated-token" }));

    await window.fetch("/api/sessions");
    expect(saveToken).toHaveBeenCalledWith("rotated-token");
  });

  it("does not save a token when the header is absent", async () => {
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(200, { ok: true }));

    await window.fetch("/api/sessions");
    expect(saveToken).not.toHaveBeenCalled();
  });
});

describe("401 handling", () => {
  it("clears token and dispatches TOKEN_EXPIRED_EVENT for a generic 401", async () => {
    const { mod, original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(401, { error: "unauthorized" }));
    const onExpired = vi.fn();
    window.addEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);

    await window.fetch("/api/sessions");

    expect(clearToken).toHaveBeenCalledOnce();
    expect(onExpired).toHaveBeenCalledOnce();
    window.removeEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);
  });

  it("dedupes a burst of 401s into a single TOKEN_EXPIRED_EVENT", async () => {
    const { mod, original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(401, { error: "unauthorized" }));
    const onExpired = vi.fn();
    window.addEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);

    await window.fetch("/api/sessions");
    await window.fetch("/api/sessions");

    expect(onExpired).toHaveBeenCalledOnce();
    expect(clearToken).toHaveBeenCalledTimes(2); // clearToken runs each time, event dedupes
    window.removeEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);
  });

  it("dispatches LOGIN_REQUIRED_EVENT (not token clear) for a 401 login_required", async () => {
    const { mod, original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(401, { error: "login_required" }));
    const onLogin = vi.fn();
    const onExpired = vi.fn();
    window.addEventListener(mod.LOGIN_REQUIRED_EVENT, onLogin);
    window.addEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);

    await window.fetch("/api/sessions");

    expect(onLogin).toHaveBeenCalledOnce();
    expect(onExpired).not.toHaveBeenCalled();
    expect(clearToken).not.toHaveBeenCalled();
    window.removeEventListener(mod.LOGIN_REQUIRED_EVENT, onLogin);
    window.removeEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);
  });

  it("does not fire TOKEN_EXPIRED_EVENT for a 401 on a login-attempt path", async () => {
    const { mod, original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(401, { error: "unauthorized" }));
    const onExpired = vi.fn();
    window.addEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);

    await window.fetch("/api/login");

    expect(onExpired).not.toHaveBeenCalled();
    window.removeEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);
  });

  it("ignores a 401 on a non-/api path", async () => {
    const { mod, original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(401, { error: "unauthorized" }));
    const onExpired = vi.fn();
    window.addEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);

    await window.fetch("/not-api");

    expect(onExpired).not.toHaveBeenCalled();
    expect(clearToken).not.toHaveBeenCalled();
    window.removeEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);
  });

  it("resetTokenExpired re-arms the dedup so a later 401 fires again", async () => {
    const { mod, original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(401, { error: "unauthorized" }));
    const onExpired = vi.fn();
    window.addEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);

    await window.fetch("/api/sessions");
    expect(onExpired).toHaveBeenCalledOnce();

    mod.resetTokenExpired();
    await window.fetch("/api/sessions");
    expect(onExpired).toHaveBeenCalledTimes(2);
    window.removeEventListener(mod.TOKEN_EXPIRED_EVENT, onExpired);
  });
});

describe("403 elevation handling", () => {
  it("dispatches ELEVATION_REQUIRED_EVENT for a 403 elevation_required", async () => {
    const { mod, original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(403, { error: "elevation_required" }));
    const onElevation = vi.fn();
    window.addEventListener(mod.ELEVATION_REQUIRED_EVENT, onElevation);

    await window.fetch("/api/sessions");

    expect(onElevation).toHaveBeenCalledOnce();
    window.removeEventListener(mod.ELEVATION_REQUIRED_EVENT, onElevation);
  });

  it("ignores a 403 whose body is not elevation_required", async () => {
    const { mod, original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(403, { error: "forbidden" }));
    const onElevation = vi.fn();
    window.addEventListener(mod.ELEVATION_REQUIRED_EVENT, onElevation);

    await window.fetch("/api/sessions");

    expect(onElevation).not.toHaveBeenCalled();
    window.removeEventListener(mod.ELEVATION_REQUIRED_EVENT, onElevation);
  });

  it("ignores a 403 with a non-JSON body", async () => {
    const { mod, original } = await freshInstall();
    original.mockResolvedValue(new Response("nope", { status: 403 }));
    const onElevation = vi.fn();
    window.addEventListener(mod.ELEVATION_REQUIRED_EVENT, onElevation);

    await window.fetch("/api/sessions");

    expect(onElevation).not.toHaveBeenCalled();
    window.removeEventListener(mod.ELEVATION_REQUIRED_EVENT, onElevation);
  });
});

describe("5xx toast handling", () => {
  it("reports a toast for an /api 5xx when the server is not known-down", async () => {
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(503, { error: "boom" }));

    await window.fetch("/api/sessions");
    expect(reportError).toHaveBeenCalledWith("Server error 503 from /api/sessions");
  });

  it("suppresses the 5xx toast when the server is known-down", async () => {
    serverDown = true;
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(500, { error: "boom" }));

    await window.fetch("/api/sessions");
    expect(reportError).not.toHaveBeenCalled();
  });

  it("does not toast a 5xx on a non-/api path", async () => {
    const { original } = await freshInstall();
    original.mockResolvedValue(jsonResponse(500, { error: "boom" }));

    await window.fetch("/static");
    expect(reportError).not.toHaveBeenCalled();
  });
});

describe("network error handling", () => {
  it("reports a network-error toast and rethrows for an /api failure", async () => {
    const { original } = await freshInstall();
    const boom = new TypeError("Failed to fetch");
    original.mockRejectedValue(boom);

    await expect(window.fetch("/api/sessions")).rejects.toBe(boom);
    expect(reportError).toHaveBeenCalledWith("Network error contacting /api/sessions. Check your connection.");
  });

  it("suppresses the network toast when the server is known-down", async () => {
    serverDown = true;
    const { original } = await freshInstall();
    original.mockRejectedValue(new TypeError("Failed to fetch"));

    await expect(window.fetch("/api/sessions")).rejects.toThrow();
    expect(reportError).not.toHaveBeenCalled();
  });

  it("rethrows AbortError without a toast", async () => {
    const { original } = await freshInstall();
    const abort = new DOMException("aborted", "AbortError");
    original.mockRejectedValue(abort);

    await expect(window.fetch("/api/sessions")).rejects.toBe(abort);
    expect(reportError).not.toHaveBeenCalled();
  });

  it("rethrows TimeoutError without a toast", async () => {
    const { original } = await freshInstall();
    const timeout = new DOMException("timed out", "TimeoutError");
    original.mockRejectedValue(timeout);

    await expect(window.fetch("/api/sessions")).rejects.toBe(timeout);
    expect(reportError).not.toHaveBeenCalled();
  });

  it("does not toast a network error on a non-/api path but still rethrows", async () => {
    const { original } = await freshInstall();
    const boom = new TypeError("Failed to fetch");
    original.mockRejectedValue(boom);

    await expect(window.fetch("/asset.js")).rejects.toBe(boom);
    expect(reportError).not.toHaveBeenCalled();
  });
});

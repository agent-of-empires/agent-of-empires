import { afterEach, describe, expect, it, vi } from "vitest";

// Guards the dev-server proxy contract that `cargo xtask dev` relies on: when
// VITE_PROXY points at a running `aoe serve`, the Vite dev server must forward
// REST (/api) and every WebSocket relay (/sessions/{id}/...ws) there, with the
// WS target switched to the ws:// scheme. See vite.config.ts.

type ProxyEntry = { target: string; ws?: boolean };

async function loadProxy(
  env: Record<string, string | undefined>,
): Promise<Record<string, ProxyEntry> | undefined> {
  vi.resetModules();
  for (const [k, v] of Object.entries(env)) {
    vi.stubEnv(k, v as string | undefined);
  }
  try {
    const mod = await import("../vite.config");
    const factory = mod.default as (e: {
      command: string;
      mode: string;
    }) => { server: { proxy?: Record<string, ProxyEntry> } };
    const cfg = await factory({ command: "serve", mode: "development" });
    return cfg.server.proxy;
  } finally {
    vi.unstubAllEnvs();
  }
}

describe("vite dev server proxy", () => {
  afterEach(() => {
    vi.resetModules();
    vi.unstubAllEnvs();
  });

  it("has no proxy when VITE_PROXY is unset", async () => {
    const proxy = await loadProxy({ VITE_PROXY: undefined });
    expect(proxy).toBeUndefined();
  });

  it("forwards /api and the /sessions WebSockets to VITE_PROXY", async () => {
    const proxy = await loadProxy({ VITE_PROXY: "http://127.0.0.1:8081" });
    expect(proxy?.["/api"].target).toBe("http://127.0.0.1:8081");
    const ws = proxy?.["^/sessions/.+/ws"];
    expect(ws?.target).toBe("ws://127.0.0.1:8081");
    expect(ws?.ws).toBe(true);
  });

  it("defaults a bare host:port to http and derives the ws target", async () => {
    const proxy = await loadProxy({ VITE_PROXY: "localhost:50106" });
    expect(proxy?.["/api"].target).toBe("http://localhost:50106");
    expect(proxy?.["^/sessions/.+/ws"].target).toBe("ws://localhost:50106");
  });
});

// @vitest-environment jsdom
import { describe, expect, it, beforeEach, afterEach, vi } from "vitest";

type Entry = { level: string; message: string; target?: string; dropped?: number } & Record<string, unknown>;

async function freshLogger(install = true) {
  vi.resetModules();
  const mod = await import("./logger");
  if (install) mod.installClientLogger();
  return mod;
}

const setProperty = (target: object, key: string, value: unknown) =>
  Object.defineProperty(target, key, { configurable: true, value });

describe("logger", () => {
  let fetchMock: ReturnType<typeof vi.fn>;
  let restoreFixture: () => void;
  const entries = (call = 0): Entry[] => JSON.parse(fetchMock.mock.calls[call]![1].body).entries;
  const flush = () => vi.advanceTimersByTimeAsync(2000);

  beforeEach(() => {
    const properties = [
      [window, "location"],
      [navigator, "sendBeacon"],
      [navigator, "userAgent"],
    ] as const;
    const descriptors = properties.map(([target, key]) => Object.getOwnPropertyDescriptor(target, key));
    const windowListeners = vi.spyOn(window, "addEventListener");
    const documentListeners = vi.spyOn(document, "addEventListener");
    restoreFixture = () => {
      for (const [type, listener, options] of windowListeners.mock.calls) {
        window.removeEventListener(type, listener, options);
      }
      for (const [type, listener, options] of documentListeners.mock.calls) {
        document.removeEventListener(type, listener, options);
      }
      properties.forEach(([target, key], index) => {
        const descriptor = descriptors[index];
        if (descriptor) Object.defineProperty(target, key, descriptor);
        else Reflect.deleteProperty(target, key);
      });
    };
    vi.useFakeTimers();
    vi.setSystemTime(0);
    fetchMock = vi.fn().mockResolvedValue(new Response("ok", { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    setProperty(window, "location", new URL("http://localhost/sessions/abc?token=secret#frag"));
    setProperty(navigator, "sendBeacon", undefined);
    setProperty(navigator, "userAgent", "vitest-agent");
  });

  afterEach(async () => {
    restoreFixture();
    await vi.runOnlyPendingTimersAsync();
    vi.clearAllTimers();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("normalizes an Error and POSTs it on flush with a sanitized path", async () => {
    const { reportError } = await freshLogger();
    reportError(new Error("boom"), { target: "test", sessionId: "s1" });
    await flush();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0]!;
    expect(url).toBe("/api/client-log");
    expect(init).toMatchObject({ method: "POST", keepalive: true, credentials: "include" });
    expect(entries()).toEqual([
      expect.objectContaining({
        level: "error",
        message: "boom",
        stack: expect.any(String),
        target: "test",
        sessionId: "s1",
        userAgent: "vitest-agent",
        path: "/sessions/abc#frag",
      }),
    ]);
  });

  it("normalizes strings, objects, circular objects, and a level override", async () => {
    const { reportError } = await freshLogger();
    const circular: Record<string, unknown> = {};
    circular.self = circular;
    reportError("plain string", { target: "t" });
    reportError({ code: 42 });
    reportError(circular);
    reportError("a warning", { level: "warn" });
    await flush();
    const [plain, object, cyclic, warning] = entries();
    expect(plain).toMatchObject({ message: "plain string", target: "t" });
    expect(object!.message).toBe('{"code":42}');
    expect(cyclic!.message).toContain("[object Object]");
    expect(warning!.level).toBe("warn");
  });

  it("flushes immediately when the batch hits MAX_BATCH", async () => {
    const { reportError } = await freshLogger(false);
    for (let i = 0; i < 20; i++) {
      if (i % 5 === 0) vi.setSystemTime((i + 1) * 1000);
      reportError(new Error(`e${i}`));
    }
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(entries()).toHaveLength(20);
  });

  it("rate-limits past the token cap and emits a dropped notice", async () => {
    const { reportError } = await freshLogger();
    for (let i = 0; i < 15; i++) reportError(new Error(`e${i}`));
    await flush();
    const batch = entries();
    expect(batch).toHaveLength(11);
    expect(batch.at(-1)).toMatchObject({
      level: "warn",
      target: "logger.relay",
      message: expect.stringContaining("dropped 5 entries"),
      dropped: 5,
    });
  });

  it("refills tokens as wall-clock advances", async () => {
    const { reportError } = await freshLogger();
    for (let i = 0; i < 10; i++) reportError(new Error(`first${i}`));
    vi.setSystemTime(1000);
    for (let i = 0; i < 5; i++) reportError(new Error(`second${i}`));
    await flush();
    expect(entries()).toHaveLength(15);
    expect(entries().some((e) => e.dropped)).toBe(false);
  });

  it("flushes via sendBeacon when hidden or on pagehide", async () => {
    const beacon = vi.fn().mockReturnValue(true);
    setProperty(navigator, "sendBeacon", beacon);
    const { reportError } = await freshLogger();
    reportError(new Error("via beacon"));
    document.dispatchEvent(new Event("visibilitychange"));
    expect(beacon).not.toHaveBeenCalled();
    vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(0);
    expect(beacon).toHaveBeenCalledWith("/api/client-log", expect.any(Blob));
    reportError(new Error("pagehide"));
    window.dispatchEvent(new Event("pagehide"));
    await vi.advanceTimersByTimeAsync(0);
    expect(beacon).toHaveBeenCalledTimes(2);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("captures window errors and unhandled rejections", async () => {
    await freshLogger();
    const withError = new Event("error") as ErrorEvent;
    Object.defineProperty(withError, "error", { value: new Error("global err") });
    const messageOnly = new Event("error") as ErrorEvent;
    Object.defineProperties(messageOnly, { error: { value: null }, message: { value: "string message only" } });
    const rejection = new Event("unhandledrejection") as PromiseRejectionEvent;
    Object.defineProperty(rejection, "reason", { value: "rejected reason" });
    for (const event of [withError, messageOnly, rejection]) window.dispatchEvent(event);
    await flush();
    expect(entries().map((e) => [e.target, e.message])).toEqual([
      ["window.onerror", "global err"],
      ["window.onerror", "string message only"],
      ["window.unhandledrejection", "rejected reason"],
    ]);
  });

  it("trims an oversized batch and re-reports the remainder as dropped", async () => {
    const { reportError } = await freshLogger();
    const chunk = "x".repeat(25 * 1024);
    reportError(chunk);
    reportError(chunk);
    await flush();
    expect(entries().map((e) => e.message.length)).toEqual([25 * 1024]);
    await flush();
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(entries(1).find((e) => e.dropped)?.dropped).toBeGreaterThanOrEqual(1);
  });

  it("falls back to '/' when window.location is unreadable", async () => {
    const { reportError } = await freshLogger();
    Object.defineProperty(window, "location", {
      configurable: true,
      get() {
        throw new Error("no location");
      },
    });
    reportError(new Error("loc fail"));
    await flush();
    expect(entries()[0]!.path).toBe("/");
  });

  it("installClientLogger is idempotent", async () => {
    const { installClientLogger } = await freshLogger();
    installClientLogger();
    window.dispatchEvent(new ErrorEvent("error", { message: "one event" }));
    window.dispatchEvent(new Event("pagehide"));
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(entries().map((e) => e.message)).toEqual(["one event"]);
  });

  it("swallows a fetch rejection and never flushes an empty queue", async () => {
    const { reportError } = await freshLogger();
    await flush();
    expect(fetchMock).not.toHaveBeenCalled();
    fetchMock.mockRejectedValueOnce(new Error("network down"));
    reportError(new Error("will fail to send"));
    await flush();
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });
});

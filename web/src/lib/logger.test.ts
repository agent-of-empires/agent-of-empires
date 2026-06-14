// @vitest-environment jsdom
//
// Unit tests for the browser-side client logger. Covers reportError
// normalization, token-bucket throttling + the dropped-entries notice,
// batch flush over fetch and sendBeacon, the byte-budget trim path, and
// the window/document listener wiring installed by installClientLogger.
//
// The module keeps process-global state (queue, token bucket, installed
// flag), so each test resets the module registry via vi.resetModules and
// re-imports a fresh copy to keep cases isolated.

import { describe, expect, it, beforeEach, afterEach, vi } from "vitest";

type LoggerModule = typeof import("./logger");

async function freshLogger(): Promise<LoggerModule> {
  vi.resetModules();
  return import("./logger");
}

function setHref(href: string): void {
  Object.defineProperty(window, "location", {
    configurable: true,
    value: new URL(href),
  });
}

describe("logger", () => {
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
    fetchMock = vi.fn().mockResolvedValue(new Response("ok", { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    setHref("http://localhost/sessions/abc?token=secret#frag");
    // sendBeacon is optional on the global; default to absent so the
    // fetch path runs unless a test opts in.
    Object.defineProperty(navigator, "sendBeacon", {
      configurable: true,
      value: undefined,
    });
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "vitest-agent",
    });
  });

  afterEach(() => {
    vi.runOnlyPendingTimers();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("normalizes an Error and POSTs it on flush", async () => {
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    reportError(new Error("boom"), { target: "test", sessionId: "s1" });

    await vi.advanceTimersByTimeAsync(2000);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("/api/client-log");
    expect(init.method).toBe("POST");
    expect(init.keepalive).toBe(true);
    expect(init.credentials).toBe("include");
    const payload = JSON.parse(init.body);
    expect(payload.entries).toHaveLength(1);
    const entry = payload.entries[0];
    expect(entry.level).toBe("error");
    expect(entry.message).toBe("boom");
    expect(entry.stack).toBeTypeOf("string");
    expect(entry.target).toBe("test");
    expect(entry.sessionId).toBe("s1");
    expect(entry.userAgent).toBe("vitest-agent");
    // Token is stripped from the path, frag/pathname preserved.
    expect(entry.path).toBe("/sessions/abc#frag");
  });

  it("normalizes a string error", async () => {
    const { reportError } = await freshLogger();
    reportError("plain string", { target: "t" });
    await vi.advanceTimersByTimeAsync(2000);
    // No installClientLogger -> no flush interval; flush manually via size.
    // Instead just assert nothing crashed; drive an explicit flush path
    // by enqueuing to the batch threshold below in another test.
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("normalizes a non-error object and a circular object", async () => {
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    reportError({ code: 42 });
    const circular: Record<string, unknown> = {};
    circular.self = circular;
    reportError(circular);
    await vi.advanceTimersByTimeAsync(2000);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const payload = JSON.parse(fetchMock.mock.calls[0][1].body);
    const messages = payload.entries.map((e: { message: string }) => e.message);
    expect(messages).toContain('{"code":42}');
    // JSON.stringify throws on the circular ref -> String(err) fallback.
    expect(messages.some((m: string) => m.includes("[object Object]"))).toBe(true);
  });

  it("respects the ctx.level override", async () => {
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    reportError("a warning", { level: "warn" });
    await vi.advanceTimersByTimeAsync(2000);
    const payload = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect(payload.entries[0].level).toBe("warn");
  });

  it("flushes immediately when the batch hits MAX_BATCH", async () => {
    const { reportError } = await freshLogger();
    // The token bucket caps at 10, so we advance wall-clock 1s every few
    // entries to refill (10/s) and let all 20 through to hit MAX_BATCH,
    // which triggers an inline flush (no installer needed).
    for (let i = 0; i < 20; i++) {
      if (i % 5 === 0) vi.setSystemTime((i + 1) * 1000);
      reportError(new Error(`e${i}`));
    }
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const payload = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect(payload.entries).toHaveLength(20);
  });

  it("rate-limits past the token cap and emits a dropped notice", async () => {
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    // Token bucket caps at 10 with no time advance, so entries 11+ drop.
    for (let i = 0; i < 15; i++) {
      reportError(new Error(`e${i}`));
    }
    await vi.advanceTimersByTimeAsync(2000);
    const payload = JSON.parse(fetchMock.mock.calls[0][1].body);
    // 10 real entries + 1 synthetic "dropped" warn entry.
    expect(payload.entries).toHaveLength(11);
    const notice = payload.entries[payload.entries.length - 1];
    expect(notice.level).toBe("warn");
    expect(notice.target).toBe("logger.relay");
    expect(notice.message).toContain("dropped 5 entries");
    expect(notice.dropped).toBe(5);
  });

  it("refills tokens as wall-clock advances", async () => {
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    for (let i = 0; i < 10; i++) reportError(new Error(`first${i}`));
    // Drain done. Advance 1s -> ~10 tokens refill (10/s).
    vi.setSystemTime(1000);
    for (let i = 0; i < 5; i++) reportError(new Error(`second${i}`));
    await vi.advanceTimersByTimeAsync(2000);
    const payload = JSON.parse(fetchMock.mock.calls[0][1].body);
    // All 15 accepted, no dropped notice.
    expect(payload.entries).toHaveLength(15);
    expect(payload.entries.some((e: { dropped?: number }) => e.dropped)).toBe(false);
  });

  it("uses sendBeacon on the hidden/pagehide path", async () => {
    const beacon = vi.fn().mockReturnValue(true);
    Object.defineProperty(navigator, "sendBeacon", {
      configurable: true,
      value: beacon,
    });
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    reportError(new Error("via beacon"));

    document.dispatchEvent(new Event("visibilitychange"));
    // visibilityState defaults to "visible" in jsdom; flush only on hidden.
    expect(beacon).not.toHaveBeenCalled();

    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      value: "hidden",
    });
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(0);

    expect(beacon).toHaveBeenCalledTimes(1);
    const [url, blob] = beacon.mock.calls[0];
    expect(url).toBe("/api/client-log");
    expect(blob).toBeInstanceOf(Blob);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("flushes via beacon on pagehide", async () => {
    const beacon = vi.fn().mockReturnValue(true);
    Object.defineProperty(navigator, "sendBeacon", {
      configurable: true,
      value: beacon,
    });
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    reportError(new Error("pagehide"));
    window.dispatchEvent(new Event("pagehide"));
    await vi.advanceTimersByTimeAsync(0);
    expect(beacon).toHaveBeenCalledTimes(1);
  });

  it("captures window.onerror and unhandledrejection", async () => {
    const { installClientLogger } = await freshLogger();
    installClientLogger();

    const errEvent = new Event("error") as ErrorEvent;
    Object.defineProperty(errEvent, "error", { value: new Error("global err") });
    window.dispatchEvent(errEvent);

    const rejEvent = new Event("unhandledrejection") as PromiseRejectionEvent;
    Object.defineProperty(rejEvent, "reason", { value: "rejected reason" });
    window.dispatchEvent(rejEvent);

    await vi.advanceTimersByTimeAsync(2000);
    const payload = JSON.parse(fetchMock.mock.calls[0][1].body);
    const targets = payload.entries.map((e: { target?: string }) => e.target);
    expect(targets).toContain("window.onerror");
    expect(targets).toContain("window.unhandledrejection");
  });

  it("falls back to e.message when ErrorEvent has no error object", async () => {
    const { installClientLogger } = await freshLogger();
    installClientLogger();
    const errEvent = new Event("error") as ErrorEvent;
    Object.defineProperty(errEvent, "error", { value: null });
    Object.defineProperty(errEvent, "message", { value: "string message only" });
    window.dispatchEvent(errEvent);
    await vi.advanceTimersByTimeAsync(2000);
    const payload = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect(payload.entries[0].message).toBe("string message only");
  });

  it("trims an oversized batch to the byte budget and re-reports the remainder as dropped", async () => {
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    // Two entries each ~25KB: the first fits under the 48KB budget, the
    // second trips it (combined ~50KB) and is counted dropped. The
    // dropped count surfaces as a synthetic notice on the following flush.
    // Use string payloads so no stack trace inflates the serialized size.
    const chunk = "x".repeat(25 * 1024);
    reportError(chunk);
    reportError(chunk);
    await vi.advanceTimersByTimeAsync(2000);
    const firstPayload = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect(firstPayload.entries).toHaveLength(1);
    expect(firstPayload.entries[0].message.length).toBe(25 * 1024);

    await vi.advanceTimersByTimeAsync(2000);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    const secondPayload = JSON.parse(fetchMock.mock.calls[1][1].body);
    const notice = secondPayload.entries.find((e: { dropped?: number }) => e.dropped);
    expect(notice).toBeTruthy();
    expect(notice.dropped).toBeGreaterThanOrEqual(1);
  });

  it("falls back to '/' when window.location is unparseable", async () => {
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    Object.defineProperty(window, "location", {
      configurable: true,
      get() {
        throw new Error("no location");
      },
    });
    reportError(new Error("loc fail"));
    await vi.advanceTimersByTimeAsync(2000);
    const payload = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect(payload.entries[0].path).toBe("/");
  });

  it("installClientLogger is idempotent", async () => {
    const { installClientLogger } = await freshLogger();
    const addSpy = vi.spyOn(window, "addEventListener");
    installClientLogger();
    const firstCount = addSpy.mock.calls.length;
    installClientLogger();
    expect(addSpy.mock.calls.length).toBe(firstCount);
  });

  it("swallows a fetch rejection without throwing", async () => {
    fetchMock.mockRejectedValueOnce(new Error("network down"));
    const { reportError, installClientLogger } = await freshLogger();
    installClientLogger();
    reportError(new Error("will fail to send"));
    // A rejected fetch is swallowed inside flush; advancing timers must
    // not surface the rejection (no unhandled promise, no throw).
    await vi.advanceTimersByTimeAsync(2000);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("does not flush when the queue is empty", async () => {
    const { installClientLogger } = await freshLogger();
    installClientLogger();
    await vi.advanceTimersByTimeAsync(2000);
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

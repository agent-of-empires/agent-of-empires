// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";

import { hydrateWebUiStateFromServer } from "../webUiSync";
import { getWebSettingsSnapshot } from "../../hooks/useWebSettings";
import { onWebUiStateHydrated } from "../webUiHydration";

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  localStorage.clear();
});

describe("bounded web UI hydration", () => {
  it("restores synced settings without AbortSignal.any and cleans up caller cancellation", async () => {
    vi.useFakeTimers();
    vi.stubGlobal(
      "AbortSignal",
      new Proxy(AbortSignal, {
        get: (target, key, receiver) => (key === "any" ? undefined : Reflect.get(target, key, receiver)),
      }),
    );
    const cached = JSON.stringify({ structuredHistoryWindow: 200 });
    const synced = JSON.stringify({ structuredHistoryWindow: 500 });
    localStorage.setItem("aoe-web-settings", cached);
    let requestSignal!: AbortSignal;
    vi.stubGlobal(
      "fetch",
      vi.fn((_url: string, init: RequestInit) => {
        requestSignal = init.signal!;
        return Promise.resolve(new Response(JSON.stringify({ "aoe-web-settings": synced })));
      }),
    );
    const controller = new AbortController();
    const removeListener = vi.spyOn(controller.signal, "removeEventListener");
    const settled = vi.fn();
    const off = onWebUiStateHydrated(settled);
    try {
      await hydrateWebUiStateFromServer(controller.signal);
      expect(getWebSettingsSnapshot().structuredHistoryWindow).toBe(500);
      expect(settled).toHaveBeenCalledTimes(1);
      expect(removeListener).toHaveBeenCalledWith("abort", expect.any(Function));
      controller.abort();
      expect(requestSignal.aborted).toBe(false);
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      off();
    }
  });

  it.each(["headers", "body"])("forwards caller cancellation while response %s stalls", async (phase) => {
    vi.useFakeTimers();
    const cached = JSON.stringify({ structuredHistoryWindow: 200 });
    localStorage.setItem("aoe-web-settings", cached);
    let requestSignal!: AbortSignal;
    let started!: () => void;
    const requested = new Promise<void>((resolve) => (started = resolve));
    vi.stubGlobal(
      "fetch",
      vi.fn((_url: string, init: RequestInit) => {
        requestSignal = init.signal!;
        if (phase === "headers") {
          return new Promise((_resolve, reject) => {
            requestSignal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), {
              once: true,
            });
            started();
          });
        }
        return Promise.resolve(
          new Response(
            new ReadableStream({
              start(controller) {
                requestSignal.addEventListener(
                  "abort",
                  () => controller.error(new DOMException("aborted", "AbortError")),
                  {
                    once: true,
                  },
                );
              },
            }),
          ),
        ).then((response) => {
          const json = response.json.bind(response);
          response.json = () => {
            started();
            return json();
          };
          return response;
        });
      }),
    );
    const controller = new AbortController();
    const removeListener = vi.spyOn(controller.signal, "removeEventListener");
    const settled = vi.fn();
    const off = onWebUiStateHydrated(settled);
    try {
      const hydration = hydrateWebUiStateFromServer(controller.signal);
      await requested;
      expect(requestSignal.aborted).toBe(false);
      controller.abort();
      await hydration;
      expect(requestSignal.aborted).toBe(true);
      expect(settled).not.toHaveBeenCalled();
      expect(localStorage.getItem("aoe-web-settings")).toBe(cached);
      expect(removeListener).toHaveBeenCalledWith("abort", expect.any(Function));
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      off();
    }
  });

  it.each(["headers", "body"])("keeps cached settings when response %s stall", async (phase) => {
    vi.useFakeTimers();
    const cached = JSON.stringify({ structuredHistoryWindow: 200 });
    localStorage.setItem("aoe-web-settings", cached);
    let signal!: AbortSignal;
    let started!: () => void;
    const requested = new Promise<void>((resolve) => (started = resolve));
    vi.stubGlobal(
      "fetch",
      vi.fn((_url: string, init: RequestInit) => {
        signal = init.signal!;
        if (phase === "headers") {
          return new Promise((_resolve, reject) => {
            signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), { once: true });
            started();
          });
        }
        return Promise.resolve(
          new Response(
            new ReadableStream({
              start(controller) {
                signal.addEventListener("abort", () => controller.error(new DOMException("aborted", "AbortError")), {
                  once: true,
                });
              },
            }),
          ),
        ).then((response) => {
          const json = response.json.bind(response);
          response.json = () => {
            started();
            return json();
          };
          return response;
        });
      }),
    );
    const settled = vi.fn();
    const off = onWebUiStateHydrated(settled);
    try {
      const caller = new AbortController();
      const hydration = hydrateWebUiStateFromServer(caller.signal);
      await requested;
      await vi.advanceTimersByTimeAsync(14_999);
      expect(settled).not.toHaveBeenCalled();
      expect(signal.aborted).toBe(false);
      await vi.advanceTimersByTimeAsync(1);
      await hydration;
      expect(signal.aborted).toBe(true);
      expect(settled).toHaveBeenCalledTimes(1);
      expect(localStorage.getItem("aoe-web-settings")).toBe(cached);
      expect(getWebSettingsSnapshot().structuredHistoryWindow).toBe(200);
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      off();
    }
  });

  it.each([false, true])("discards cancelled mount responses (already aborted: %s)", async (alreadyAborted) => {
    let respond!: (response: Response) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        () =>
          new Promise<Response>((resolve) => {
            respond = resolve;
          }),
      ),
    );
    const controller = new AbortController();
    if (alreadyAborted) controller.abort();
    const settled = vi.fn();
    const off = onWebUiStateHydrated(settled);
    try {
      const hydration = hydrateWebUiStateFromServer(controller.signal);
      controller.abort();
      respond(new Response(JSON.stringify({ "aoe-web-settings": JSON.stringify({ structuredHistoryWindow: 500 }) })));
      await hydration;
      expect(localStorage.getItem("aoe-web-settings")).toBeNull();
      expect(settled).not.toHaveBeenCalled();
    } finally {
      off();
    }
  });
});

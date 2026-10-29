// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";

import { hydrateWebUiStateFromServer } from "../webUiSync";
import { getWebSettingsSnapshot } from "../../hooks/useWebSettings";
import { onWebUiStateHydrated } from "../webUiHydration";

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  localStorage.clear();
});

describe("bounded web UI hydration", () => {
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
      const hydration = hydrateWebUiStateFromServer();
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

  it("discards an old authenticated mount's response after cancellation", async () => {
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

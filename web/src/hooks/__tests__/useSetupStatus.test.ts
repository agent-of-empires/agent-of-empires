// @vitest-environment jsdom
import { act, renderHook, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, expect, it, vi } from "vitest";
import { notifySettingsChanged } from "../../lib/settingsEvents";
import { toastBus } from "../../lib/toastBus";
import { useSetupStatus } from "../useSetupStatus";

const server = setupServer();
beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  toastBus.handler = null;
});
afterAll(() => server.close());

const status = (configured: boolean) => ({
  finished: true,
  deferred: false,
  banner_dismissed: false,
  tmux: true,
  git: true,
  access: { serve_mode: "local" },
  steps: [{ key: "sandbox", required: false, configured, confirmed: false, skipped: false }],
});

it("re-reads setup status after a settings save", async () => {
  let configured = false;
  server.use(http.get("*/api/setup", () => HttpResponse.json(status(configured))));
  const { result } = renderHook(() => useSetupStatus(true));
  await waitFor(() => expect(result.current.status?.steps[0]?.configured).toBe(false));
  configured = true;
  act(() => notifySettingsChanged());
  await waitFor(() => expect(result.current.status?.steps[0]?.configured).toBe(true));
});

it("reports a failed update with the server's message", async () => {
  const error = vi.fn();
  toastBus.handler = { push: vi.fn(), error, info: vi.fn(), openLink: vi.fn(), action: vi.fn() };
  server.use(
    http.get("*/api/setup", () => HttpResponse.json(status(false))),
    http.patch("*/api/setup", () => HttpResponse.json({ message: "Disk full" }, { status: 400 })),
  );
  const { result } = renderHook(() => useSetupStatus(true));
  await act(() => result.current.update({ banner_dismissed: true }));
  expect(error).toHaveBeenCalledWith("Disk full");
});

// @vitest-environment jsdom
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, expect, it } from "vitest";
import { ThemeTelemetryStep } from "../steps/ThemeTelemetryStep";

const server = setupServer();
beforeAll(() => server.listen({ onUnhandledRequest: "bypass" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

it("a superseded theme pick neither rolls back the newer pick nor reports an error", async () => {
  const pending: Record<string, () => void> = {};
  server.use(
    http.get("*/api/themes", () => HttpResponse.json(["empire", "dracula", "nord"])),
    http.patch("*/api/theme", async ({ request }) => {
      const { name } = (await request.json()) as { name: string };
      await new Promise<void>((resolve) => (pending[name] = resolve));
      return HttpResponse.json({ ok: true });
    }),
  );
  render(
    <ThemeTelemetryStep
      share={false}
      onShareChange={() => {}}
      doNotTrack={false}
      telemetry="loaded"
      onRetryTelemetry={() => {}}
    />,
  );
  fireEvent.click(await screen.findByRole("radio", { name: "dracula" }));
  await waitFor(() => expect(pending.dracula).toBeDefined());
  fireEvent.click(screen.getByRole("radio", { name: "nord" }));
  await waitFor(() => expect(pending.nord).toBeDefined());

  await act(async () => pending.nord!());
  await act(async () => pending.dracula!());

  await waitFor(() => expect((screen.getByRole("radio", { name: "nord" }) as HTMLInputElement).checked).toBe(true));
  expect(screen.queryByText(/Couldn't/)).toBeNull();
});

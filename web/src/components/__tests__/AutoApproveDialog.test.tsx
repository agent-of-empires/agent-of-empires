// @vitest-environment jsdom
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { AutoApproveDialog } from "../AutoApproveDialog";
import { toastBus } from "../../lib/toastBus";

const server = setupServer();
beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterAll(() => server.close());
afterEach(() => {
  cleanup();
  server.resetHandlers();
  toastBus.handler = null;
});

function mount() {
  const onClose = vi.fn();
  const info = vi.fn();
  const error = vi.fn();
  toastBus.handler = { info, error, push: vi.fn(), openLink: vi.fn() };
  render(<AutoApproveDialog sessionId="s1" sessionTitle="Working session" onClose={onClose} />);
  return { onClose, info, error };
}

describe("mid-session auto-approve", () => {
  it("prevents duplicate changes and stays pending until the agent acknowledges", async () => {
    for (const enabled of [true, false]) {
      let acknowledge!: () => void;
      const ack = new Promise<void>((resolve) => {
        acknowledge = resolve;
      });
      server.use(
        http.post("/api/sessions/s1/auto-approve", async () => {
          await ack;
          return HttpResponse.json({ enabled, restarted: enabled });
        }),
      );
      const { onClose, info } = mount();
      fireEvent.click(screen.getByRole("button", { name: `Turn ${enabled ? "on" : "off"} auto-approve` }));
      await screen.findByRole("status");
      expect(onClose).not.toHaveBeenCalled();
      expect(info).not.toHaveBeenCalled();
      for (const button of screen.getAllByRole("button")) {
        expect((button as HTMLButtonElement).disabled).toBe(true);
      }
      acknowledge();
      await waitFor(() => expect(onClose).toHaveBeenCalledOnce());
      expect(info).toHaveBeenCalledOnce();
      cleanup();
      server.resetHandlers();
    }
  });

  it("keeps rejected changes visible and never claims success", async () => {
    for (const [status, body, message] of [
      [409, { message: "Cannot resume this agent" }, "Cannot resume this agent"],
      [403, { message: "Server is read-only" }, "Server is read-only"],
      [403, { message: "Disabled by CityHall" }, "Disabled by CityHall"],
      [502, "Adapter unavailable", "Adapter unavailable"],
      [200, {}, "Agent did not acknowledge the auto-approve change"],
      [200, { enabled: false, restarted: false }, "Agent did not acknowledge the auto-approve change"],
    ] as const) {
      server.use(
        http.post("/api/sessions/s1/auto-approve", () =>
          typeof body === "string" ? HttpResponse.text(body, { status }) : HttpResponse.json(body, { status }),
        ),
      );
      const { onClose, info, error } = mount();
      fireEvent.click(screen.getByRole("button", { name: "Turn on auto-approve" }));
      await waitFor(() => expect(screen.getByRole("alert").textContent).toBe(message));
      expect(error).toHaveBeenCalledWith(message);
      expect(info).not.toHaveBeenCalled();
      expect(onClose).not.toHaveBeenCalled();
      expect((screen.getByRole("button", { name: "Turn off auto-approve" }) as HTMLButtonElement).disabled).toBe(false);
      cleanup();
      server.resetHandlers();
    }
  });
});

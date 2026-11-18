// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, expect, it } from "vitest";
import { SandboxStep } from "../steps/SandboxStep";
import { descriptor } from "../../settings/__tests__/fixtures";

const server = setupServer();
beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

it("a failed probe asks to check again instead of reporting no runtime, then shows the Daemon's start command", async () => {
  let fail = true;
  server.use(
    http.get("*/api/setup/runtimes", () =>
      fail
        ? new HttpResponse(null, { status: 500 })
        : HttpResponse.json([
            { kind: "docker", installed: true, running: false, start_command: "sudo systemctl start docker" },
          ]),
    ),
  );
  render(<SandboxStep schema={[]} values={{}} onSaveField={async () => true} />);
  expect((await screen.findByRole("alert")).textContent).toContain("Couldn't check container runtimes");
  expect(screen.queryByText(/No container runtime found/)).toBeNull();

  fail = false;
  fireEvent.click(screen.getByRole("button", { name: /Check again/ }));
  expect(await screen.findByText("sudo systemctl start docker")).toBeTruthy();
  expect(screen.queryByRole("alert")).toBeNull();
});

it("a successful probe with nothing installed says so", async () => {
  server.use(
    http.get("*/api/setup/runtimes", () =>
      HttpResponse.json([{ kind: "docker", installed: false, running: false, start_command: "x" }]),
    ),
  );
  render(<SandboxStep schema={[]} values={{}} onSaveField={async () => true} />);
  expect(await screen.findByText(/No container runtime found/)).toBeTruthy();
});

it("shows the sandbox settings without their config keys", async () => {
  server.use(
    http.get("*/api/setup/runtimes", () =>
      HttpResponse.json([{ kind: "docker", installed: true, running: true, start_command: "x" }]),
    ),
  );
  const schema = [
    descriptor({
      section: "sandbox",
      field: "enabled_by_default",
      label: "Run new Sessions in a sandbox",
      summary: "Each Session's Agent runs inside its own container.",
      web_write: { policy: "requires_elevation", reason: "r" },
    }),
  ];
  render(<SandboxStep schema={schema} values={{}} onSaveField={async () => true} />);
  expect(await screen.findByRole("switch", { name: "Run new Sessions in a sandbox" })).toBeTruthy();
  expect(screen.getByText("Each Session's Agent runs inside its own container.")).toBeTruthy();
  expect(screen.getByRole("img", { name: "Asks for the passphrase" })).toBeTruthy();
  expect(screen.queryByText("sandbox.enabled_by_default")).toBeNull();
  expect(screen.queryByRole("button", { name: "Copy config key" })).toBeNull();
});

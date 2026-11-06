// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, expect, it } from "vitest";
import { AgentsStep } from "../steps/AgentsStep";

const server = setupServer();
beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const claude = {
  name: "claude",
  display_name: "Claude Code",
  kind: "builtin",
  installed: true,
  acp_capable: true,
  acp_installed: true,
  acp_allowed: false,
  install_hint: "npm i -g claude",
};

const radio = (name: RegExp | string) => screen.getByRole("radio", { name }) as HTMLInputElement;

it("shows a saved Off view as Off, a policy-blocked adapter as not allowed, and no unsaved default Agent", async () => {
  server.use(http.get("*/api/agents", () => HttpResponse.json([claude])));
  render(
    <AgentsStep
      profile=""
      defaultTool="gone"
      structuredView="off"
      save={async () => true}
      onAgentsChecked={() => {}}
    />,
  );
  expect(await screen.findByText("Structured view not allowed")).toBeTruthy();
  expect(screen.queryByText("Structured view ready")).toBeNull();
  expect(radio(/Terminal only/).checked).toBe(true);
  expect(radio(/^Structured view/).checked).toBe(false);
  expect(radio(/^claude/).checked).toBe(false);
});

it("a failed Agent check asks to check again instead of reporting none installed", async () => {
  let fail = true;
  server.use(
    http.get("*/api/agents", () =>
      fail ? new HttpResponse(null, { status: 500 }) : HttpResponse.json([{ ...claude, acp_allowed: true }]),
    ),
  );
  render(
    <AgentsStep
      profile=""
      defaultTool="claude"
      structuredView={undefined}
      save={async () => true}
      onAgentsChecked={() => {}}
    />,
  );
  expect((await screen.findByRole("alert")).textContent).toContain("Couldn't check for Agents");
  expect(screen.queryByText(/No Agents found/)).toBeNull();
  fail = false;
  fireEvent.click(screen.getByRole("button", { name: /Check again/ }));
  expect(await screen.findByText("Structured view ready")).toBeTruthy();
  expect(radio(/^claude/).checked).toBe(true);
});

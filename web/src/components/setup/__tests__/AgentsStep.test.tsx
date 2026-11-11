// @vitest-environment jsdom
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, expect, it } from "vitest";
import { useInlineMachineSave } from "../../../hooks/useInlineMachineSave";
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

it("a pick cancelled by a newer one at the passphrase prompt doesn't roll back the newer pick", async () => {
  const attempts: unknown[] = [];
  const committed: unknown[] = [];
  let elevated = false;
  server.use(
    http.get("*/api/agents", () =>
      HttpResponse.json([claude, { ...claude, name: "codex", display_name: "Codex CLI" }]),
    ),
    http.patch("*/api/settings", async ({ request }) => {
      const body = await request.json();
      if (!elevated) {
        attempts.push(body);
        return HttpResponse.json({ error: "elevation_required" }, { status: 403 });
      }
      committed.push(body);
      return new HttpResponse(null, { status: 204 });
    }),
    http.post("*/api/login/elevate", () => {
      elevated = true;
      return HttpResponse.json({ elevated_until_secs: 900 });
    }),
  );
  function Harness() {
    const { save, elevation } = useInlineMachineSave();
    return (
      <>
        <AgentsStep
          profile=""
          defaultTool={undefined}
          structuredView={undefined}
          save={(section, field, value) => save({ [section]: { [field]: value } })}
          onAgentsChecked={() => {}}
        />
        {elevation && <button onClick={() => void elevation.submit("right")}>Unlock</button>}
      </>
    );
  }
  render(<Harness />);
  fireEvent.click(await screen.findByRole("radio", { name: /^claude/ }));
  await screen.findByRole("button", { name: "Unlock" });
  fireEvent.click(radio(/^codex/));
  await waitFor(() => expect(attempts).toHaveLength(2));
  fireEvent.click(await screen.findByRole("button", { name: "Unlock" }));

  await waitFor(() => expect(committed).toEqual([{ session: { default_tool: "codex" } }]));
  await waitFor(() => expect(screen.queryByRole("button", { name: "Unlock" })).toBeNull());
  expect(radio(/^codex/).checked).toBe(true);
});

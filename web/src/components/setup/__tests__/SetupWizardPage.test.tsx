// @vitest-environment jsdom
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { useState } from "react";
import { MemoryRouter } from "react-router-dom";
import { afterAll, afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { resetSettingsSchemaCache, type SetupStatus, type SetupStepKey } from "../../../lib/api";
import type { SettingsFieldDescriptor } from "../../../lib/types";
import { SetupWizardPage } from "../SetupWizardPage";

const server = setupServer();
beforeAll(() => server.listen({ onUnhandledRequest: "bypass" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

let patches: unknown[];
beforeEach(() => {
  resetSettingsSchemaCache();
  patches = [];
  server.use(
    http.get("*/api/settings/schema", () => HttpResponse.json([])),
    http.get("*/api/settings", () => HttpResponse.json({})),
    http.get("*/api/telemetry/status", () =>
      HttpResponse.json({ enabled: false, responded: false, do_not_track: false }),
    ),
    http.get("*/api/themes", () => HttpResponse.json(["empire"])),
    http.get("*/api/agents", () => HttpResponse.json([])),
    http.get("*/api/setup/runtimes", () => HttpResponse.json([])),
    http.get("*/api/projects", () => HttpResponse.json([])),
    http.patch("*/api/setup", async ({ request }) => {
      patches.push(await request.json());
      return new HttpResponse(null, { status: 204 });
    }),
  );
});

const FIELD: SettingsFieldDescriptor = {
  section: "worktree",
  field: "enabled",
  category: "worktree",
  label: "Use worktrees",
  description: "",
  widget: { kind: "toggle" },
  web_write: { policy: "allow" },
  profile_overridable: true,
  validation: { rule: "bool" },
  advanced: false,
};

const KEYS: SetupStepKey[] = ["theme_telemetry", "agents", "project", "worktrees", "sandbox", "notifications"];
const status = (configured: SetupStepKey[] = []): SetupStatus => ({
  finished: false,
  deferred: false,
  banner_dismissed: false,
  tmux: true,
  git: true,
  access: { serve_mode: "local" },
  steps: KEYS.map((key) => ({
    key,
    required: key === "agents",
    configured: configured.includes(key),
    confirmed: false,
    skipped: false,
  })),
});

/** Settings-backed steps keep Next disabled until saved settings load. */
async function enabled(name: string) {
  const button = screen.getByRole("button", { name }) as HTMLButtonElement;
  await waitFor(() => expect(button.disabled).toBe(false));
  return button;
}

function renderPage(current: SetupStatus, entry = "/setup") {
  const onLeave = vi.fn();
  render(
    <MemoryRouter initialEntries={[entry]}>
      <SetupWizardPage status={current} about={null} profile="" refresh={async () => current} onLeave={onLeave} />
    </MemoryRouter>,
  );
  return { onLeave };
}

it("opens on the first open step and never offers Skip on the required Agents step", () => {
  renderPage(status(["theme_telemetry"]));
  expect(screen.getByRole("heading", { name: "Agents" })).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Skip for now" })).toBeNull();
  expect(screen.getByText("Required to start Sessions")).toBeTruthy();
  expect((screen.getByRole("button", { name: "Next" }) as HTMLButtonElement).disabled).toBe(true);
});

it("Skip records the step and moves on", async () => {
  renderPage(status(["agents"]), "/setup?step=sandbox");
  fireEvent.click(screen.getByRole("button", { name: "Skip for now" }));
  await waitFor(() => expect(patches).toEqual([{ skipped_steps: ["sandbox"] }]));
  expect(await screen.findByRole("heading", { name: "Notifications and access" })).toBeTruthy();
});

it("Finish sends the user back to the open required step instead of finishing", async () => {
  const { onLeave } = renderPage(status(), "/setup?step=notifications");
  fireEvent.click(await enabled("Finish setup"));
  expect(await screen.findByRole("alert")).toBeTruthy();
  expect(screen.getByRole("heading", { name: "Agents" })).toBeTruthy();
  expect(patches).not.toContainEqual({ finished: true });
  expect(onLeave).not.toHaveBeenCalled();
});

it("Finish keeps the last step's defaults and marks setup finished once Agents is configured", async () => {
  const { onLeave } = renderPage(status(["agents"]), "/setup?step=notifications");
  fireEvent.click(await enabled("Finish setup"));
  await waitFor(() => expect(onLeave).toHaveBeenCalled());
  expect(patches).toEqual([{ confirmed_steps: ["notifications"] }, { finished: true }]);
});

it("Go to dashboard defers setup so / stops redirecting", async () => {
  const { onLeave } = renderPage(status());
  fireEvent.click(screen.getAllByRole("button", { name: /Go to dashboard/ })[0]!);
  await waitFor(() => expect(onLeave).toHaveBeenCalled());
  expect(patches).toEqual([{ deferred: true }]);
});

it("keeps earlier skips and confirmations as later steps are saved", async () => {
  let serverStatus = status(["agents"]);
  server.use(
    http.patch("*/api/setup", async ({ request }) => {
      const body = (await request.json()) as { skipped_steps?: string[]; confirmed_steps?: string[] };
      patches.push(body);
      serverStatus = {
        ...serverStatus,
        steps: serverStatus.steps.map((s) => {
          const confirmed = body.confirmed_steps ? body.confirmed_steps.includes(s.key) : s.confirmed;
          const skipped = body.skipped_steps ? body.skipped_steps.includes(s.key) : s.skipped && !confirmed;
          return { ...s, confirmed, skipped, configured: s.key === "agents" || confirmed };
        }),
      };
      return new HttpResponse(null, { status: 204 });
    }),
  );
  function Harness() {
    const [current, setCurrent] = useState(serverStatus);
    const refresh = async () => {
      setCurrent(serverStatus);
      return serverStatus;
    };
    return <SetupWizardPage status={current} about={null} profile="" refresh={refresh} onLeave={() => {}} />;
  }
  render(
    <MemoryRouter initialEntries={["/setup?step=project"]}>
      <Harness />
    </MemoryRouter>,
  );
  fireEvent.click(await enabled("Next"));
  await screen.findByRole("heading", { name: "Worktrees" });
  fireEvent.click(screen.getByRole("button", { name: "Skip for now" }));
  await screen.findByRole("heading", { name: "Sandbox" });
  fireEvent.click(await enabled("Next"));
  await screen.findByRole("heading", { name: "Notifications and access" });
  expect(patches).toEqual([
    { confirmed_steps: ["project"] },
    { skipped_steps: ["worktrees"] },
    { confirmed_steps: ["project", "sandbox"] },
  ]);
});

it("stays on the step and reports it when keeping the defaults fails to save", async () => {
  server.use(http.patch("*/api/setup", () => HttpResponse.json({ message: "Disk full" }, { status: 500 })));
  renderPage(status(["agents"]), "/setup?step=sandbox");
  fireEvent.click(await enabled("Next"));
  expect((await screen.findByRole("alert")).textContent).toBe("Disk full");
  expect(screen.getByRole("heading", { name: "Sandbox" })).toBeTruthy();
});

it("telemetry waits for the saved choice, then Next saves it instead of confirming the step", async () => {
  let answer!: () => void;
  const consent: unknown[] = [];
  server.use(
    http.get(
      "*/api/telemetry/status",
      () =>
        new Promise<Response>(
          (resolve) =>
            (answer = () => resolve(HttpResponse.json({ enabled: true, responded: true, do_not_track: false }))),
        ),
    ),
    http.post("*/api/telemetry/consent", async ({ request }) => {
      consent.push(await request.json());
      return HttpResponse.json({ enabled: true, responded: true, do_not_track: false });
    }),
  );
  renderPage(status(["agents"]), "/setup?step=theme_telemetry");
  await waitFor(() => expect(answer).toBeDefined());
  expect(screen.getByText("Loading your current choice…")).toBeTruthy();
  expect((screen.getByRole("button", { name: "Next" }) as HTMLButtonElement).disabled).toBe(true);

  await act(async () => answer());
  const sharing = await screen.findByRole("radio", { name: /Share anonymous usage counts/ });
  expect((sharing as HTMLInputElement).checked).toBe(true);
  fireEvent.click(await enabled("Next"));
  await screen.findByRole("heading", { name: "Agents" });
  expect(consent).toEqual([{ enabled: true }]);
  expect(patches).toEqual([]);
});

it("a failed telemetry read offers a retry and keeps Next disabled", async () => {
  let fail = true;
  server.use(
    http.get("*/api/telemetry/status", () =>
      fail
        ? new HttpResponse(null, { status: 500 })
        : HttpResponse.json({ enabled: false, responded: true, do_not_track: false }),
    ),
  );
  renderPage(status(["agents"]), "/setup?step=theme_telemetry");
  expect((await screen.findByRole("alert")).textContent).toContain("Couldn't load your current choice");
  expect((screen.getByRole("button", { name: "Next" }) as HTMLButtonElement).disabled).toBe(true);
  fail = false;
  fireEvent.click(screen.getByRole("button", { name: "Check again" }));
  expect(await screen.findByRole("radio", { name: /Not now/ })).toBeTruthy();
  expect((screen.getByRole("button", { name: "Next" }) as HTMLButtonElement).disabled).toBe(false);
});

it("a failed settings read offers a retry instead of showing defaults as the saved choices", async () => {
  let fail = true;
  server.use(
    http.get("*/api/settings", () => (fail ? new HttpResponse(null, { status: 500 }) : HttpResponse.json({}))),
  );
  renderPage(status(["agents"]), "/setup?step=worktrees");
  expect((await screen.findByRole("alert")).textContent).toContain("Couldn't load your settings");
  expect((screen.getByRole("button", { name: "Next" }) as HTMLButtonElement).disabled).toBe(true);
  fail = false;
  fireEvent.click(screen.getByRole("button", { name: "Check again" }));
  await enabled("Next");
  expect(screen.queryByRole("alert")).toBeNull();
});

it("a failed schema read offers a retry instead of a step with no fields", async () => {
  let fail = true;
  let release: (() => void) | undefined;
  server.use(
    http.get("*/api/settings/schema", async () => {
      if (fail) return new HttpResponse(null, { status: 500 });
      await new Promise<void>((resolve) => (release = resolve));
      return HttpResponse.json([FIELD]);
    }),
  );
  renderPage(status(["agents"]), "/setup?step=worktrees");
  expect((await screen.findByRole("alert")).textContent).toContain("Couldn't load the settings for this step");
  const next = screen.getByRole("button", { name: "Next" }) as HTMLButtonElement;
  expect(next.disabled).toBe(true);
  fail = false;
  fireEvent.click(screen.getByRole("button", { name: "Check again" }));
  await waitFor(() => expect(release).toBeDefined());
  // The retry is still out: confirming now would accept defaults the user never saw.
  expect(next.disabled).toBe(true);
  await act(async () => release!());
  await enabled("Next");
  expect(screen.getByText(FIELD.label)).toBeTruthy();
  expect(screen.queryByRole("alert")).toBeNull();
});

it("stays on setup when the reload after Go to dashboard fails", async () => {
  const onLeave = vi.fn();
  render(
    <MemoryRouter initialEntries={["/setup"]}>
      <SetupWizardPage
        status={status(["agents"])}
        about={null}
        profile=""
        refresh={async () => null}
        onLeave={onLeave}
      />
    </MemoryRouter>,
  );
  fireEvent.click(screen.getAllByRole("button", { name: /Go to dashboard/ })[0]!);
  expect((await screen.findByRole("alert")).textContent).toContain("couldn't reload setup");
  expect(patches).toEqual([{ deferred: true }]);
  expect(onLeave).not.toHaveBeenCalled();
});

const agent = (name: string) => ({
  name,
  display_name: name,
  kind: "builtin",
  installed: true,
  acp_capable: true,
  acp_installed: true,
  acp_allowed: false,
  install_hint: "",
});

/** Agents step with claude, codex and gemini; saving gemini is rejected. */
function agentPicks() {
  const committed: unknown[] = [];
  const current = status(["agents"]);
  const refresh = { hold: false, release: undefined as (() => void) | undefined };
  server.use(
    http.get("*/api/agents", () => HttpResponse.json(["claude", "codex", "gemini"].map(agent))),
    http.patch("*/api/settings", async ({ request }) => {
      const body = (await request.json()) as { session: { default_tool: string } };
      if (body.session.default_tool === "gemini") return HttpResponse.json({ error: "invalid" }, { status: 400 });
      committed.push(body);
      return new HttpResponse(null, { status: 204 });
    }),
  );
  render(
    <MemoryRouter initialEntries={["/setup?step=agents"]}>
      <SetupWizardPage
        status={current}
        about={null}
        profile=""
        refresh={async () => {
          if (refresh.hold) {
            refresh.hold = false;
            await new Promise<void>((resolve) => (refresh.release = resolve));
          }
          return current;
        }}
        onLeave={() => {}}
      />
    </MemoryRouter>,
  );
  const radio = (name: string) => screen.getByRole("radio", { name: new RegExp(`^${name}`) }) as HTMLInputElement;
  return { committed, refresh, radio };
}

it("a slow status read after an older save doesn't make a failed pick roll back to it", async () => {
  const { committed, refresh, radio } = agentPicks();
  await screen.findByRole("radio", { name: /^claude/ });
  refresh.hold = true;
  fireEvent.click(radio("claude"));
  await waitFor(() => expect(refresh.release).toBeDefined());
  fireEvent.click(radio("codex"));
  await waitFor(() => expect(committed).toHaveLength(2));
  await act(async () => refresh.release!());

  fireEvent.click(radio("gemini"));
  await waitFor(() => expect(radio("codex").checked).toBe(true));
});

it("a failed pick rolls back to the older pick that saved", async () => {
  const { committed, radio } = agentPicks();
  fireEvent.click(await screen.findByRole("radio", { name: /^claude/ }));
  await waitFor(() => expect(committed).toHaveLength(1));
  fireEvent.click(radio("gemini"));
  await waitFor(() => expect(radio("claude").checked).toBe(true));
});

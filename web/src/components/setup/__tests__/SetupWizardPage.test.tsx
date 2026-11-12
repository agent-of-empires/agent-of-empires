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
  await waitFor(() => expect(patches).toEqual([{ skip_step: "sandbox" }]));
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
  expect(patches).toEqual([{ confirm_step: "notifications" }, { finished: true }]);
});

it("Go to dashboard defers setup so / stops redirecting", async () => {
  const { onLeave } = renderPage(status());
  fireEvent.click(screen.getAllByRole("button", { name: /Go to dashboard/ })[0]!);
  await waitFor(() => expect(onLeave).toHaveBeenCalled());
  expect(patches).toEqual([{ deferred: true }]);
});

type StepDelta = { skip_step?: string; confirm_step?: string; unskip_step?: string };

/** A mock `/api/setup` that applies step deltas like the Daemon; `hold` delays one PATCH before it applies. */
function deltaServer(initial: SetupStatus) {
  const state = { status: initial, saved: false, hold: undefined as ((body: StepDelta) => boolean) | undefined };
  const release = { held: undefined as (() => void) | undefined };
  server.use(
    http.patch("*/api/setup", async ({ request }) => {
      const body = (await request.json()) as StepDelta;
      patches.push(body);
      if (state.hold?.(body)) {
        state.hold = undefined;
        await new Promise<void>((resolve) => (release.held = resolve));
      }
      state.status = {
        ...state.status,
        steps: state.status.steps.map((s) => {
          const confirmed = s.key === body.confirm_step || (s.confirmed && s.key !== body.skip_step);
          const skipped =
            s.key === body.skip_step || (s.skipped && s.key !== body.confirm_step && s.key !== body.unskip_step);
          return { ...s, confirmed, skipped, configured: s.configured || confirmed };
        }),
      };
      return new HttpResponse(null, { status: 204 });
    }),
  );
  function Harness() {
    const [current, setCurrent] = useState(state.status);
    const refresh = async () => {
      setCurrent(state.status);
      return state.status;
    };
    return <SetupWizardPage status={current} about={null} profile="" refresh={refresh} onLeave={() => {}} />;
  }
  return { state, release, Harness };
}

it("keeps earlier skips and confirmations as later steps are saved", async () => {
  const { state, Harness } = deltaServer(status(["agents"]));
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
  expect(patches).toEqual([{ confirm_step: "project" }, { skip_step: "worktrees" }, { confirm_step: "sandbox" }]);
  const flags = (key: SetupStepKey) => state.status.steps.find((s) => s.key === key);
  expect(flags("project")?.confirmed && flags("sandbox")?.confirmed && flags("worktrees")?.skipped).toBe(true);
});

it("a late un-skip from a save doesn't erase a skip made on another step meanwhile", async () => {
  const initial = status(["agents"]);
  initial.steps = initial.steps.map((s) => (s.key === "worktrees" ? { ...s, skipped: true } : s));
  const { state, release, Harness } = deltaServer(initial);
  server.use(
    http.get("*/api/settings/schema", () => HttpResponse.json([FIELD])),
    http.patch("*/api/settings", () => {
      state.status = {
        ...state.status,
        steps: state.status.steps.map((s) => (s.key === "worktrees" ? { ...s, configured: true } : s)),
      };
      return new HttpResponse(null, { status: 204 });
    }),
  );
  state.hold = (body) => Object.keys(body).length > 0 && body.skip_step === undefined;
  render(
    <MemoryRouter initialEntries={["/setup?step=worktrees"]}>
      <Harness />
    </MemoryRouter>,
  );
  fireEvent.click(await screen.findByRole("switch"));
  await waitFor(() => expect(release.held).toBeDefined());

  fireEvent.click(screen.getAllByRole("button", { name: /Sandbox/ })[0]!);
  await screen.findByRole("heading", { name: "Sandbox" });
  fireEvent.click(screen.getByRole("button", { name: "Skip for now" }));
  await screen.findByRole("heading", { name: "Notifications and access" });
  await act(async () => release.held!());

  await waitFor(() => expect(patches).toHaveLength(2));
  await waitFor(() => {
    const skipped = state.status.steps.filter((s) => s.skipped).map((s) => s.key);
    expect(skipped).toEqual(["sandbox"]);
  });
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

/**
 * Worktrees step with `enabled` and `auto_cleanup` toggles. Saving `enabled` gets `enabledReply`; the
 * re-read that follows is held and answers `reread`. `read` resolves once that answer is parsed and
 * every microtask after it, including the wizard's, has run.
 */
function heldReread(enabledReply: () => Response, reread: Record<string, unknown>) {
  const committed: unknown[] = [];
  const reads = { count: 0, release: undefined as (() => void) | undefined };
  server.use(
    http.get("*/api/settings/schema", () =>
      HttpResponse.json([FIELD, { ...FIELD, field: "auto_cleanup", label: "Clean up worktrees" }]),
    ),
    http.get("*/api/settings", async () => {
      if (++reads.count !== 2) return HttpResponse.json({ worktree: { enabled: false, auto_cleanup: false } });
      await new Promise<void>((resolve) => (reads.release = resolve));
      return HttpResponse.json({ worktree: reread });
    }),
    http.patch("*/api/settings", async ({ request }) => {
      const body = (await request.json()) as { worktree: Record<string, unknown> };
      if ("enabled" in body.worktree) return enabledReply();
      committed.push(body);
      return new HttpResponse(null, { status: 204 });
    }),
  );
  let parsed!: () => void;
  const read = new Promise<void>((resolve) => (parsed = resolve));
  const realFetch = globalThis.fetch;
  const fetchSpy = vi.spyOn(globalThis, "fetch").mockImplementation(async (...args) => {
    const res = await realFetch(...args);
    if (reads.count !== 2 || (args[1]?.method ?? "GET") !== "GET") return res;
    const json = res.json.bind(res);
    res.json = () => json().finally(() => setTimeout(parsed));
    return res;
  });

  /** Saves `enabled`, then commits `auto_cleanup` while the re-read is held, then releases it. */
  const race = async () => {
    renderPage(status(["agents"]), "/setup?step=worktrees");
    await enabled("Next");
    const [enabledSwitch, cleanup] = screen.getAllByRole("switch");
    fireEvent.click(enabledSwitch!);
    await waitFor(() => expect(reads.release).toBeDefined());
    fireEvent.click(cleanup!);
    await waitFor(() => expect(committed).toEqual([{ worktree: { auto_cleanup: true } }]));
    await waitFor(() => expect(cleanup!.getAttribute("aria-checked")).toBe("true"));
    await act(async () => {
      reads.release!();
      await read;
    });
    return { enabledSwitch: enabledSwitch!, cleanup: cleanup! };
  };
  return { race, restore: () => fetchSpy.mockRestore() };
}

it("a stale read after a rejected save doesn't undo a newer save that committed", async () => {
  const { race, restore } = heldReread(() => HttpResponse.json({ error: "invalid" }, { status: 400 }), {
    enabled: false,
    auto_cleanup: false,
  });
  try {
    const { cleanup } = await race();
    expect(cleanup.getAttribute("aria-checked")).toBe("true");
  } finally {
    restore();
  }
});

it("an uncertain save that landed shows its value even after another field saves", async () => {
  const { race, restore } = heldReread(() => HttpResponse.json({ error: "boom" }, { status: 500 }), {
    enabled: true,
    auto_cleanup: true,
  });
  try {
    const { enabledSwitch } = await race();
    // No passphrase prompt replaced the footer.
    expect(screen.getByRole("button", { name: "Next" })).toBeTruthy();
    expect(enabledSwitch.getAttribute("aria-checked")).toBe("true");
  } finally {
    restore();
  }
});

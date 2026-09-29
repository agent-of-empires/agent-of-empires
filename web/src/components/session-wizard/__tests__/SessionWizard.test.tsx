// @vitest-environment jsdom

import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { SessionWizard, type WizardPrefill } from "../SessionWizard";
import { fetchAgents, fetchCreateProgress, fetchIsGitRepo, fetchProfiles, fetchSettings } from "../../../lib/api";
import { agent } from "./fixtures";
import { toastBus } from "../../../lib/toastBus";
import { startPendingCreates } from "../../../lib/pendingCreates";

const createSession = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchCreateProgress: vi.fn().mockResolvedValue(null),
  fetchSettings: vi.fn().mockResolvedValue({}),
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchIsGitRepo: vi.fn().mockResolvedValue(true),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: vi.fn().mockResolvedValue({ available: false }),
  fetchProfiles: vi.fn().mockResolvedValue([]),
  fetchVolumeIgnoresPreview: vi.fn().mockResolvedValue([]),
  markVolumeIgnoresGlobsAcknowledged: vi.fn().mockResolvedValue(undefined),
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [] }),
  // One recent keeps ProjectStep on the Recent tab instead of the directory browser.
  fetchRecentProjects: vi.fn().mockResolvedValue({
    projects: [{ path: "/tmp/proj", display_name: "proj", tool: "claude", last_used_at: "2026-01-01T00:00:00Z" }],
  }),
  fetchProjects: vi.fn().mockResolvedValue([]),
  createSession: (...args: unknown[]) => createSession(...args),
}));

const INSTRUCTION_KEY = "aoe-new-session-last-instruction";

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
  vi.mocked(fetchIsGitRepo).mockImplementation(async (path) => path !== "/tmp/plain");
});

afterEach(() => {
  cleanup();
  localStorage.clear();
});

function renderWizard(prefill: WizardPrefill = { path: "/tmp/proj", tool: "claude" }) {
  const onCreated = vi.fn();
  const onClose = vi.fn();
  const onCreatedInBackground = vi.fn();
  render(
    <SessionWizard
      onClose={onClose}
      onCreated={onCreated}
      onCreatedInBackground={onCreatedInBackground}
      prefill={prefill}
    />,
  );
  return { onCreated, onClose, onCreatedInBackground };
}

// Launch stays disabled until the profile defaults settle, as for a real click.
const launch = async () => {
  const button = screen.getByText(/Launch session/).closest("button") as HTMLButtonElement;
  await waitFor(() => expect(button.disabled).toBe(false));
  fireEvent.click(button);
};
const payload = (call = 0) => createSession.mock.calls[call]![0];

describe("SessionWizard structured view payload", () => {
  it.each([
    [false, "structured"],
    [true, "terminal"],
  ])("opting out=%s sends view %s", async (optOut, view) => {
    renderWizard();
    if (optOut) fireEvent.click(screen.getByRole("switch", { name: "Use structured view" }));
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload()).toMatchObject({ tool: "claude", view });
  });

  it.each([
    ["terminal", "terminal"],
    ["auto", "structured"],
    ["structured", "structured"],
  ])("opens on and sends the configured default view %s (#3517)", async (setting, view) => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({ acp: { default_new_session_view: setting } } as never);
    renderWizard();
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload()).toMatchObject({ tool: "claude", view });
  });

  it("sends profile-resolved agent model and effort defaults", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({
      session: { default_tool: "opencode", acp_defaults: { opencode: { model: "openai/gpt-5.5", effort: "high" } } },
      sandbox: {},
    } as never);
    renderWizard({ path: "/tmp/proj" });
    // The Agent row names opencode once the defaults have applied.
    await waitFor(() => expect(screen.getByTestId("wizard-agent-row").textContent).toContain("opencode"));
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload()).toMatchObject({
      tool: "opencode",
      view: "structured",
      agent_model: "openai/gpt-5.5",
      agent_effort: "high",
    });
  });
});

describe("SessionWizard last instruction memory", () => {
  it("prefills the stored instruction into the create payload", async () => {
    localStorage.setItem(INSTRUCTION_KEY, "always be terse");
    const { onCreated } = renderWizard();
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(payload()).toMatchObject({ custom_instruction: "always be terse" });
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
  });

  it.each(["review for security", ""])("stores the submitted instruction %j", async (text) => {
    localStorage.setItem(INSTRUCTION_KEY, "stale text");
    renderWizard();
    fireEvent.click(screen.getByTestId("wizard-agent-row"));
    fireEvent.change(screen.getByPlaceholderText("Custom instructions for this session..."), {
      target: { value: text },
    });
    fireEvent.click(screen.getByRole("button", { name: "Done" }));
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(payload().custom_instruction).toBe(text || undefined);
    await waitFor(() => expect(localStorage.getItem(INSTRUCTION_KEY)).toBe(text));
  });
});

describe("SessionWizard hooks trust", () => {
  const REFUSAL = {
    ok: false,
    error: "Repository hooks require trust.",
    hooksNeedTrust: {
      onCreate: ["bash scripts/setup-worktree.sh"],
      onLaunch: ["npm start"],
      onDestroy: [],
      needsMcpTrust: false,
    },
  };
  const openDialog = async () => {
    await launch();
    await waitFor(() => expect(screen.getByTestId("hooks-trust-dialog")).toBeTruthy());
  };

  it("pauses on the trust dialog, then resubmits with trust_hooks on Proceed", async () => {
    createSession.mockResolvedValueOnce(REFUSAL).mockResolvedValueOnce({ ok: true, session: { id: "s1" } });
    const { onCreated } = renderWizard();
    await openDialog();
    expect(screen.getByTestId("hooks-trust-list").textContent).toContain("bash scripts/setup-worktree.sh");
    expect(screen.getByTestId("hooks-trust-list").textContent).toContain("npm start");
    expect(payload()).not.toHaveProperty("trust_hooks", true);
    fireEvent.click(screen.getByTestId("hooks-trust-proceed"));
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
    expect(payload(1)).toMatchObject({ trust_hooks: true });
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
  });

  it("Cancel dismisses the dialog without a second submit", async () => {
    createSession.mockResolvedValue(REFUSAL);
    renderWizard();
    await openDialog();
    fireEvent.click(screen.getByText("Cancel"));
    await waitFor(() => expect(screen.queryByTestId("hooks-trust-dialog")).toBeNull());
    expect(createSession).toHaveBeenCalledTimes(1);
  });

  it("shows the error instead of looping when a trusted retry is refused again", async () => {
    createSession.mockResolvedValue(REFUSAL);
    renderWizard();
    await openDialog();
    fireEvent.click(screen.getByTestId("hooks-trust-proceed"));
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(screen.getByText("Repository hooks require trust.")).toBeTruthy());
  });
});

describe("SessionWizard rows", () => {
  const viewSwitch = () => screen.getByRole("switch", { name: "Use structured view" }) as HTMLButtonElement;

  it.each([
    ["claude", undefined, false, /plan, tool calls and diffs/],
    ["aider", [agent("aider", { acp_capable: false })], true, /no ACP adapter/],
    ["helper", [agent("helper", { kind: "custom", acp_capable: false })], true, /needs agent_acp_cmd/],
    ["claude", [agent("claude", { acp_allowed: false })], true, /not on the allowed agents list/],
  ])("gates the structured view for %s", async (tool, agents, disabled, summary) => {
    if (agents) vi.mocked(fetchAgents).mockResolvedValueOnce(agents);
    renderWizard({ path: "/tmp/proj", tool });
    await waitFor(() => expect(viewSwitch().disabled).toBe(disabled));
    expect(viewSwitch().closest("div")!.textContent).toMatch(summary);
  });

  it.each([
    [{ scratch: true }, "not for scratch sessions"],
    [{ path: "/tmp/plain" }, "not a git repository"],
  ])("disables the worktree switch with a reason: %o", async (prefill, reason) => {
    renderWizard(prefill);
    await waitFor(() => expect(screen.getByTestId("wizard-worktree-row").textContent).toContain(reason));
    expect((screen.getByRole("switch", { name: "Create a worktree" }) as HTMLButtonElement).disabled).toBe(true);
  });

  // A blocked switch left on shows on, matching the create payload, and can be turned off.
  it.each([
    [[agent("settl", { host_only: true })], "Run in a safe container", "settl runs on the host only"],
    [undefined, "Run in a safe container", "Docker is not running"],
    [[agent("settl", { host_only: true })], "Create a worktree", "settl runs on the host only"],
  ])("a blocked switch left on shows on and turns off: %#", async (agents, name, reason) => {
    if (agents) vi.mocked(fetchAgents).mockResolvedValueOnce(agents);
    const tool = agents ? "settl" : "claude";
    renderWizard({ path: "/tmp/proj", tool, sandboxEnabled: true, worktreeEnabled: true });
    const toggle = () => screen.getByRole("switch", { name }) as HTMLButtonElement;
    await waitFor(() => expect(toggle().closest("div")!.textContent).toContain(reason));
    expect(toggle().getAttribute("aria-checked")).toBe("true");
    fireEvent.click(toggle());
    await waitFor(() => expect(toggle().disabled).toBe(true));
    expect(toggle().getAttribute("aria-checked")).toBe("false");
  });

  it("opens the project picker with no project, and returns to the form on a pick", async () => {
    renderWizard({});
    fireEvent.click((await screen.findByText("/tmp/proj")).closest("button")!);
    expect(screen.getByTestId("wizard-project-row").textContent).toContain("/tmp/proj");
  });

  describe("profile", () => {
    const PROFILES = [
      { name: "default", is_default: true, description: "Stock setup" },
      { name: "work", is_default: false },
    ];

    it("picks a profile in its panel and applies its defaults", async () => {
      vi.mocked(fetchProfiles).mockResolvedValue(PROFILES);
      renderWizard();
      fireEvent.click(await screen.findByText("Profile"));
      fireEvent.click(screen.getByRole("radio", { name: /work/ }));
      await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("work"));
      await launch();
      await waitFor(() => expect(createSession).toHaveBeenCalled());
      expect(payload()).toMatchObject({ profile: "work" });
    });

    it("confirms before a profile overwrites edits", async () => {
      vi.mocked(fetchProfiles).mockResolvedValue(PROFILES);
      const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(false);
      try {
        renderWizard();
        fireEvent.click(await screen.findByRole("switch", { name: "Auto-approve actions" }));
        fireEvent.click(screen.getByText("Profile"));
        fireEvent.click(screen.getByRole("radio", { name: /work/ }));
        expect(confirmSpy).toHaveBeenCalled();
        expect(fetchSettings).not.toHaveBeenCalledWith("work");
      } finally {
        confirmSpy.mockRestore();
      }
    });
  });
});

describe("SessionWizard under StrictMode", () => {
  // The app root renders in StrictMode, whose dev setup/cleanup/setup must not leave the
  // wizard thinking it was closed.
  it("completes an ordinary Launch in the foreground", async () => {
    const onCreated = vi.fn();
    const onCreatedInBackground = vi.fn();
    render(
      <StrictMode>
        <SessionWizard
          onClose={vi.fn()}
          onCreated={onCreated}
          onCreatedInBackground={onCreatedInBackground}
          prefill={{ path: "/tmp/proj", tool: "claude" }}
        />
      </StrictMode>,
    );
    await launch();
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
    expect(onCreatedInBackground).not.toHaveBeenCalled();
  });
});

describe("SessionWizard create progress", () => {
  const deferred = () => {
    let resolve!: (v: unknown) => void;
    const promise = new Promise((r) => (resolve = r));
    return { promise, resolve };
  };

  it("shows hook output while a slow create runs, polled by its idempotency key", async () => {
    const pending = deferred();
    createSession.mockReturnValueOnce(pending.promise);
    vi.mocked(fetchCreateProgress).mockResolvedValue({
      stage: "running_hooks",
      hook: "npm install",
      output: ["added 12 packages"],
    });
    const { onCreated } = renderWizard();
    await launch();
    const key = payload().idempotency_key;
    expect(key).toBeTruthy();
    await waitFor(() => expect(screen.getByTestId("create-progress-hook").textContent).toContain("npm install"), {
      timeout: 3000,
    });
    expect(screen.getByTestId("create-progress-output").textContent).toContain("added 12 packages");
    expect(fetchCreateProgress).toHaveBeenCalledWith(key);
    pending.resolve({ ok: true, session: { id: "s1" } });
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
  });

  it("hands a backgrounded create to onCreatedInBackground instead of navigating", async () => {
    const pending = deferred();
    createSession.mockReturnValueOnce(pending.promise);
    const { onClose, onCreated, onCreatedInBackground } = renderWizard();
    await launch();
    fireEvent.click(await screen.findByText("Continue in background", undefined, { timeout: 3000 }));
    expect(onClose).toHaveBeenCalled();
    pending.resolve({ ok: true, session: { id: "s1" } });
    await waitFor(() => expect(onCreatedInBackground).toHaveBeenCalledWith({ id: "s1" }));
    expect(onCreated).not.toHaveBeenCalled();
  });

  it("retries a dropped request with the same idempotency key", async () => {
    createSession
      .mockResolvedValueOnce({ ok: false, error: "Network error", network: true })
      .mockResolvedValueOnce({ ok: true, session: { id: "s1" } });
    const { onCreated } = renderWizard();
    await launch();
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }), { timeout: 3000 });
    expect(createSession).toHaveBeenCalledTimes(2);
    expect(payload(1).idempotency_key).toBe(payload(0).idempotency_key);
  });
});

describe("SessionWizard unknown create outcome", () => {
  const LOST = { ok: false, error: "Network error: offline", network: true };
  const toastError = vi.fn();

  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    toastBus.handler = { push: vi.fn(), error: toastError, info: vi.fn(), openLink: vi.fn() };
  });
  afterEach(() => {
    vi.useRealTimers();
    toastBus.handler = null;
    toastError.mockReset();
  });

  // Every response lost: the server may still have created the session.
  const loseEveryResponse = async () => {
    createSession.mockResolvedValue(LOST);
    await launch();
    await vi.advanceTimersByTimeAsync(60_000);
  };

  const DAY_MS = 24 * 60 * 60 * 1000;

  it("stops without sending once the replay window passes while the browser is offline", async () => {
    let online = true;
    Object.defineProperty(window.navigator, "onLine", { configurable: true, get: () => online });
    try {
      renderWizard();
      createSession.mockImplementation(async () => {
        online = false;
        return LOST;
      });
      await launch();
      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));

      // A day offline, then the connection returns.
      vi.setSystemTime(Date.now() + DAY_MS + 1_000);
      online = true;
      window.dispatchEvent(new Event("online"));
      await vi.advanceTimersByTimeAsync(10_000);

      await waitFor(() => expect(screen.getByText(/Gave up waiting/)).toBeTruthy());
      expect(createSession).toHaveBeenCalledTimes(1);
    } finally {
      Object.defineProperty(window.navigator, "onLine", { configurable: true, get: () => true });
    }
  });

  it("does not resend an unknown outcome from Launch once its replay window has passed", async () => {
    renderWizard();
    await loseEveryResponse();
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());
    const sent = createSession.mock.calls.length;

    vi.setSystemTime(Date.now() + DAY_MS);
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await launch();
    await waitFor(() => expect(screen.getByText(/Gave up waiting/)).toBeTruthy());
    expect(createSession).toHaveBeenCalledTimes(sent);

    // The expired request is gone, so the next Launch is a fresh one.
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(sent + 1));
    expect(payload(sent).idempotency_key).not.toBe(payload(0).idempotency_key);
  });

  it("keeps the key and retries the same request from Launch", async () => {
    const { onCreated } = renderWizard();
    await loseEveryResponse();
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());
    const calls = createSession.mock.calls.length;
    expect(new Set(createSession.mock.calls.map(([body]) => body.idempotency_key)).size).toBe(1);

    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await launch();
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
    expect(payload(calls).idempotency_key).toBe(payload(0).idempotency_key);
  });

  it("reconciles under the same key when the connection returns", async () => {
    const { onCreated } = renderWizard();
    await loseEveryResponse();
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    window.dispatchEvent(new Event("online"));
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
    expect(payload(createSession.mock.calls.length - 1).idempotency_key).toBe(payload(0).idempotency_key);
  });

  // The wizard really unmounts on close, as in App, so only a longer-lived owner can keep the key.
  const launchInBackgroundAndClose = async () => {
    createSession.mockResolvedValue(LOST);
    const onCreatedInBackground = vi.fn();
    const view = render(
      <SessionWizard
        onClose={() => view.unmount()}
        onCreated={vi.fn()}
        onCreatedInBackground={onCreatedInBackground}
        prefill={{ path: "/tmp/proj", tool: "claude" }}
      />,
    );
    await launch();
    fireEvent.click(await screen.findByText("Continue in background", undefined, { timeout: 3000 }));
    expect(screen.queryByTestId("session-wizard")).toBeNull();
    await vi.advanceTimersByTimeAsync(5 * 60_000);
    const key = payload(0).idempotency_key;
    expect(new Set(createSession.mock.calls.map(([body]) => body.idempotency_key))).toEqual(new Set([key]));
    expect(toastError).not.toHaveBeenCalled();
    return { key, onCreatedInBackground };
  };

  it("a reopened wizard retries a closed wizard's unresolved create under its key", async () => {
    const { key } = await launchInBackgroundAndClose();
    const { onCreated } = renderWizard({ path: "/tmp/other", tool: "claude" });
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());

    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await launch();
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
    expect(payload(createSession.mock.calls.length - 1).idempotency_key).toBe(key);
  });

  it("the app-level owner reconciles a closed wizard's create once the server answers", async () => {
    const onCreatedByOwner = vi.fn();
    startPendingCreates({ onCreated: onCreatedByOwner, onFailed: vi.fn() });
    const { key } = await launchInBackgroundAndClose();

    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await vi.advanceTimersByTimeAsync(60_000);
    await waitFor(() => expect(onCreatedByOwner).toHaveBeenCalledWith({ id: "s1" }, expect.anything()));
    expect(payload(createSession.mock.calls.length - 1).idempotency_key).toBe(key);
    // Resolved, so a later wizard starts a fresh request.
    renderWizard();
    expect(screen.queryByText(/may still be created/)).toBeNull();
  });
});

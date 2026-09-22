// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { SessionWizard, type WizardPrefill } from "../SessionWizard";
import { fetchSettings } from "../../../lib/api";

const createSession = vi.fn();

vi.mock("../../../lib/api", () => ({
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
const MORE_OPTIONS_KEY = "aoe-new-session-more-options-open";

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
});

afterEach(() => {
  cleanup();
  localStorage.clear();
});

function renderWizard(prefill: WizardPrefill = { path: "/tmp/proj", tool: "claude" }) {
  const onCreated = vi.fn();
  render(<SessionWizard onClose={() => {}} onCreated={onCreated} prefill={prefill} />);
  return { onCreated };
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
    if (optOut) {
      fireEvent.click(screen.getByText("More options"));
      fireEvent.click(screen.getByRole("switch", { name: "Use structured view" }));
    }
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
    fireEvent.click(screen.getByText("More options"));
    // The resolved launch command shows opencode once the defaults have applied.
    await waitFor(() => expect(screen.getAllByText(/opencode/).length).toBeGreaterThan(0));
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
    localStorage.setItem(MORE_OPTIONS_KEY, "true");
    renderWizard();
    fireEvent.change(screen.getByPlaceholderText("Custom instructions for this session..."), {
      target: { value: text },
    });
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

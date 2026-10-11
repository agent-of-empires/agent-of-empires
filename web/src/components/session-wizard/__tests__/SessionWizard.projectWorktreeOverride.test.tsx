// @vitest-environment jsdom
//
// Paths the wizard opens on without a ProjectStep selection: the sidebar's "+" quick-create
// (`prefill.worktreeEnabled`) and the remembered last-used project.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { SessionWizard, type WizardPrefill } from "../SessionWizard";

async function clickLaunch(getByText: (m: RegExp) => HTMLElement) {
  const button = getByText(/Launch session/).closest("button") as HTMLButtonElement;
  await waitFor(() => expect(button.disabled).toBe(false));
  fireEvent.click(button);
}

const createSession = vi.fn();
const fetchSettings = vi.fn();
const fetchProjects = vi.fn();
const fetchDockerStatus = vi.fn();
const fetchProfiles = vi.fn();
const fetchRecentProjects = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchCreateProgress: vi.fn().mockResolvedValue(null),
  fetchCreateBootId: vi.fn().mockResolvedValue("boot-1"),
  fetchSettings: (...args: unknown[]) => fetchSettings(...args),
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchIsGitRepo: vi.fn().mockResolvedValue(true),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: (...args: unknown[]) => fetchDockerStatus(...args),
  fetchProfiles: (...args: unknown[]) => fetchProfiles(...args),
  fetchVolumeIgnoresPreview: vi.fn().mockResolvedValue({ acknowledged: true, globs: [] }),
  markVolumeIgnoresGlobsAcknowledged: vi.fn().mockResolvedValue(undefined),
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [] }),
  fetchRecentProjects: (...args: unknown[]) => fetchRecentProjects(...args),
  fetchProjects: (...args: unknown[]) => fetchProjects(...args),
  fetchProjectRegistry: (...args: unknown[]) => fetchProjects(...args),
  getHomePath: vi.fn().mockResolvedValue(null),
  createSession: (...args: unknown[]) => createSession(...args),
}));

function renderWizard(prefill?: WizardPrefill) {
  return render(<SessionWizard onClose={() => {}} onCreated={() => {}} prefill={prefill} />);
}

afterEach(() => {
  cleanup();
  localStorage.clear();
});

describe("SessionWizard prefill.worktreeEnabled (project override on quick-create)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    fetchProjects.mockResolvedValue([]);
    fetchDockerStatus.mockResolvedValue({ available: false });
    fetchProfiles.mockResolvedValue([]);
    fetchRecentProjects.mockResolvedValue({ projects: [] });
  });

  it("applies the project's override even against a conflicting global default", async () => {
    // Conflicting values, so ignoring the override cannot pass by coincidence.
    let resolveSettings!: (settings: unknown) => void;
    fetchSettings.mockReturnValue(new Promise((resolve) => (resolveSettings = resolve)));
    const { getByText } = renderWizard({ path: "/repo/alpha", worktreeEnabled: false });

    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    await act(async () => resolveSettings({ worktree: { enabled: true } }));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", worktree_enabled: false });
  });

  it.each([
    ["settings before projects", true],
    ["projects before settings", false],
  ])("applies the override to a remembered last-used path (%s)", async (_, settingsFirst) => {
    localStorage.setItem("aoe-new-session-last-project", "/repo/alpha");
    let resolveSettings!: (settings: unknown) => void;
    let resolveProjects!: (projects: unknown) => void;
    fetchSettings.mockReturnValue(new Promise((resolve) => (resolveSettings = resolve)));
    fetchProjects.mockReturnValue(new Promise((resolve) => (resolveProjects = resolve)));
    const { getByText } = renderWizard();

    const projects = [
      { name: "alpha", path: "/repo/alpha", scope: "global", pinned: false, overrides: { worktree_enabled: false } },
    ];
    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    await waitFor(() => expect(fetchProjects).toHaveBeenCalled());
    const settings = () => resolveSettings({ worktree: { enabled: true } });
    await act(async () => (settingsFirst ? settings() : resolveProjects(projects)));
    await act(async () => (settingsFirst ? resolveProjects(projects) : settings()));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", worktree_enabled: false });
  });
});

describe("project sandbox override on the remembered last-used path", () => {
  const alpha = [
    { name: "alpha", path: "/repo/alpha", scope: "global", pinned: false, overrides: { sandbox_enabled: true } },
  ];

  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    localStorage.setItem("aoe-new-session-last-project", "/repo/alpha");
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    fetchSettings.mockResolvedValue({ sandbox: { enabled_by_default: false } });
    fetchProjects.mockResolvedValue(alpha);
    fetchProfiles.mockResolvedValue([]);
    fetchRecentProjects.mockResolvedValue({ projects: [] });
  });

  it("starts the session sandboxed when Docker is available", async () => {
    fetchDockerStatus.mockResolvedValue({ available: true });
    const { getByText } = renderWizard();
    await waitFor(() => expect(fetchProjects).toHaveBeenCalled());
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", sandbox: true });
  });

  describe("when the project registry cannot be loaded", () => {
    const recent = (...paths: string[]) => ({
      projects: paths.map((path) => ({
        path,
        display_name: path.split("/").pop(),
        last_used_at: null,
        tool: "claude",
      })),
    });
    const UNRESOLVED = /Could not load this project's settings/;

    beforeEach(() => {
      fetchDockerStatus.mockResolvedValue({ available: true });
    });

    it("keeps the resolved override when the picker reload fails and the same project is reselected", async () => {
      fetchProjects.mockResolvedValueOnce(alpha).mockResolvedValue(null);
      fetchRecentProjects.mockResolvedValue(recent("/repo/alpha"));
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledTimes(1));
      const launchButton = () => getByText(/Launch session/).closest("button") as HTMLButtonElement;
      await waitFor(() => expect(launchButton().disabled).toBe(false));

      fireEvent.click(await findByText("Project"));
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledTimes(2));
      fireEvent.click((await findByText("/repo/alpha")).closest("button")!);
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", sandbox: true });
    });

    it("holds Launch after a failed initial lookup until the project is selected again", async () => {
      fetchProjects.mockResolvedValueOnce(null).mockResolvedValue(alpha);
      fetchRecentProjects.mockResolvedValue(recent("/repo/alpha"));
      const { getByText, findByText } = renderWizard();
      await findByText(UNRESOLVED);
      expect((getByText(/Launch session/).closest("button") as HTMLButtonElement).disabled).toBe(true);
      expect(createSession).not.toHaveBeenCalled();

      fireEvent.click(await findByText("Project"));
      fireEvent.click((await findByText("/repo/alpha")).closest("button")!);
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", sandbox: true });
    });

    describe("a failed same-project reselect while the initial lookup is still pending", () => {
      const reselectAlphaWithFailedPicker = async () => {
        let settleInitial!: (value: unknown) => void;
        fetchProjects
          .mockImplementationOnce(() => new Promise((resolve) => (settleInitial = resolve)))
          .mockResolvedValue(null);
        fetchRecentProjects.mockResolvedValue(recent("/repo/alpha"));
        const view = renderWizard();
        await waitFor(() => expect(fetchProjects).toHaveBeenCalledTimes(1));
        fireEvent.click(await view.findByText("Project"));
        await waitFor(() => expect(fetchProjects).toHaveBeenCalledTimes(2));
        fireEvent.click((await view.findByText("/repo/alpha")).closest("button")!);
        await view.findByText(UNRESOLVED);
        return { ...view, settleInitial: (value: unknown) => act(async () => settleInitial(value)) };
      };

      it("nothing was ever resolved, so the project stays unresolved when the initial lookup fails too", async () => {
        const { getByText, settleInitial } = await reselectAlphaWithFailedPicker();
        await settleInitial(null);

        expect((getByText(/Launch session/).closest("button") as HTMLButtonElement).disabled).toBe(true);
        expect(createSession).not.toHaveBeenCalled();
      });

      it("resolves once the initial lookup succeeds", async () => {
        const { getByText, settleInitial } = await reselectAlphaWithFailedPicker();
        await settleInitial(alpha);
        await clickLaunch(getByText);

        await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
        expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", sandbox: true });
      });

      it("applies the late seed when the remembered path only differs by a trailing slash", async () => {
        localStorage.setItem("aoe-new-session-last-project", "/repo/alpha/");
        const { getByText, settleInitial } = await reselectAlphaWithFailedPicker();
        await settleInitial(alpha);
        await clickLaunch(getByText);

        await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
        expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", sandbox: true });
      });
    });

    it("does not let a late failed mount lookup unresolve a project selected successfully since", async () => {
      let failInitial!: (value: unknown) => void;
      const beta = [
        { name: "beta", path: "/repo/beta", scope: "global", pinned: false, overrides: { sandbox_enabled: true } },
      ];
      fetchProjects
        .mockImplementationOnce(() => new Promise((resolve) => (failInitial = resolve)))
        .mockResolvedValue(beta);
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledTimes(1));

      fireEvent.click(await findByText("Project"));
      fireEvent.click((await findByText("/repo/beta")).closest("button")!);
      await act(async () => failInitial(null));
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/beta", sandbox: true });
    });

    it("does not carry the previous project's override to another project whose lookup failed", async () => {
      fetchProjects.mockResolvedValueOnce(alpha).mockResolvedValue(null);
      fetchRecentProjects.mockResolvedValue(recent("/repo/beta"));
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledTimes(1));
      const launchButton = () => getByText(/Launch session/).closest("button") as HTMLButtonElement;
      await waitFor(() => expect(launchButton().disabled).toBe(false));

      fireEvent.click(await findByText("Project"));
      fireEvent.click((await findByText("/repo/beta")).closest("button")!);

      await findByText(UNRESOLVED);
      expect(launchButton().disabled).toBe(true);
      expect(createSession).not.toHaveBeenCalled();
    });
  });

  it("refuses to submit a seeded sandbox while Docker is unavailable", async () => {
    fetchDockerStatus.mockResolvedValue({ available: false });
    const { getByText, findByText } = renderWizard();
    await waitFor(() => expect(fetchProjects).toHaveBeenCalled());
    await clickLaunch(getByText);

    await findByText(/Sandbox unavailable: Docker is not running/);
    expect(createSession).not.toHaveBeenCalled();
  });

  it("holds Launch until a slow Docker probe answers, then submits the sandbox", async () => {
    let resolveDocker!: (status: unknown) => void;
    fetchDockerStatus.mockReturnValue(new Promise((resolve) => (resolveDocker = resolve)));
    const { getByText } = renderWizard();
    await waitFor(() => expect(fetchProjects).toHaveBeenCalled());
    const button = getByText(/Launch session/).closest("button") as HTMLButtonElement;
    await act(async () => {});
    expect(button.disabled).toBe(true);

    await act(async () => resolveDocker({ available: true }));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", sandbox: true });
  });

  it("re-reads the project's overrides for the profile picked in the wizard", async () => {
    fetchDockerStatus.mockResolvedValue({ available: true });
    fetchProfiles.mockResolvedValue([
      { name: "a", is_default: true },
      { name: "b", is_default: false },
    ]);
    // The same project is registered per profile: off for the served profile "a", on for "b".
    fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
      Promise.resolve([
        {
          name: "alpha",
          path: "/repo/alpha",
          scope: "profile",
          pinned: false,
          overrides: { sandbox_enabled: profile === "b" },
        },
      ]),
    );
    const { getByText, getByRole, findByText } = renderWizard();
    await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));

    fireEvent.click(await findByText("Profile"));
    fireEvent.click(getByRole("radio", { name: /b/ }));
    await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, "b"));
    await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("b"));
    await clickLaunch(getByText);

    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/alpha", profile: "b", sandbox: true });
  });

  describe("switching profile in the wizard", () => {
    const registry = (sandbox: boolean) => [
      { name: "alpha", path: "/repo/alpha", scope: "profile", pinned: false, overrides: { sandbox_enabled: sandbox } },
    ];
    const deferred = () => {
      let resolve!: (value: unknown) => void;
      const promise = new Promise((r) => (resolve = r));
      return { promise, resolve };
    };
    const pickProfile = async (findByText: (m: string) => Promise<HTMLElement>, name: RegExp) => {
      fireEvent.click(await findByText("Profile"));
      fireEvent.click(screen.getByRole("radio", { name }));
    };

    beforeEach(() => {
      fetchDockerStatus.mockResolvedValue({ available: true });
      fetchProfiles.mockResolvedValue([
        { name: "a", is_default: true },
        { name: "b", is_default: false },
        { name: "c", is_default: false },
      ]);
    });

    it("holds Launch until the picked profile's overrides have landed", async () => {
      const b = deferred();
      fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
        profile === "b" ? b.promise : Promise.resolve(registry(false)),
      );
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));
      const launchButton = () => getByText(/Launch session/).closest("button") as HTMLButtonElement;
      await waitFor(() => expect(launchButton().disabled).toBe(false));

      await pickProfile(findByText, /^b/);
      await waitFor(() => expect(launchButton().disabled).toBe(true));

      await act(async () => b.resolve(registry(true)));
      await clickLaunch(getByText);
      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ profile: "b", sandbox: true });
    });

    it("ignores the response of a profile that was superseded before it answered", async () => {
      const b = deferred();
      const c = deferred();
      fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
        profile === "b" ? b.promise : profile === "c" ? c.promise : Promise.resolve(registry(false)),
      );
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));

      await pickProfile(findByText, /^b/);
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, "b"));
      await pickProfile(findByText, /^c/);
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, "c"));
      await act(async () => c.resolve(registry(true)));
      await act(async () => b.resolve(registry(false)));
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ profile: "c", sandbox: true });
    });

    it("ignores the mount-time registry response once a profile was picked", async () => {
      const initial = deferred();
      fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
        profile === "b" ? Promise.resolve(registry(true)) : initial.promise,
      );
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));

      await pickProfile(findByText, /^b/);
      await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("b"));
      await act(async () => initial.resolve(registry(false)));
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ profile: "b", sandbox: true });
    });

    it("keeps the previous profile whole when the new profile's project request fails", async () => {
      // Off for the served profile, and the failed profile would sandbox by default: a mix of the two
      // (profile b with a's off override) must never be submitted.
      fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
        Promise.resolve(profile === "b" ? null : registry(false)),
      );
      fetchSettings.mockImplementation((profile?: string) =>
        Promise.resolve({ sandbox: { enabled_by_default: profile === "b" } }),
      );
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));

      await pickProfile(findByText, /^b/);
      await findByText(/Could not load project settings for profile b/);
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      const body = createSession.mock.calls[0][0];
      expect(body).toMatchObject({ sandbox: false });
      expect(body.profile).not.toBe("b");
    });

    it("does not let late mount settings from a superseded profile become the scratch sandbox default", async () => {
      const initialSettings = deferred();
      // The remembered project has no registered overrides, so only the profile defaults decide the sandbox.
      fetchProjects.mockResolvedValue([]);
      fetchSettings.mockImplementation((profile?: string) =>
        profile === "b" ? Promise.resolve({ sandbox: { enabled_by_default: true } }) : initialSettings.promise,
      );
      const { getByText, findByText, getByRole } = renderWizard();
      await waitFor(() => expect(fetchSettings).toHaveBeenCalled());

      await pickProfile(findByText, /^b/);
      await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("b"));
      await waitFor(() =>
        expect(getByRole("switch", { name: "Run in a safe container" }).getAttribute("aria-checked")).toBe("true"),
      );
      // An unrelated edit makes the profile fields dirty, so the late settings take their dirty branch.
      fireEvent.click(getByRole("switch", { name: "Auto-approve actions" }));
      await act(async () => initialSettings.resolve({ sandbox: { enabled_by_default: false } }));

      fireEvent.click(getByText("Project"));
      fireEvent.click(await findByText("Scratch"));
      fireEvent.click(await findByText("Use a scratch folder"));
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ profile: "b", scratch: true, sandbox: true });
    });

    describe("back to the server default profile", () => {
      let failServedProfile = false;
      beforeEach(() => {
        failServedProfile = false;
        fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
          Promise.resolve(profile === "b" ? registry(true) : failServedProfile ? null : registry(false)),
        );
      });
      const pickServerDefault = (findByText: (m: string) => Promise<HTMLElement>) =>
        findByText("Profile").then((row) => {
          fireEvent.click(row);
          fireEvent.click(screen.getByRole("radio", { name: /Server default/ }));
        });

      it("re-reads the served profile's overrides", async () => {
        const { getByText, findByText } = renderWizard();
        await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));
        await pickProfile(findByText, /^b/);
        await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("b"));
        fetchProjects.mockClear();

        await pickServerDefault(findByText);
        await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));
        await clickLaunch(getByText);

        await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
        const body = createSession.mock.calls[0][0];
        expect(body).toMatchObject({ path: "/repo/alpha", sandbox: false });
        expect(body.profile).not.toBe("b");
      });

      it("names the default profile when its project request fails", async () => {
        const { findByText } = renderWizard();
        await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));
        await pickProfile(findByText, /^b/);
        await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("b"));
        failServedProfile = true;

        await pickServerDefault(findByText);
        await findByText(/Could not load project settings for profile default/);
      });
    });

    it("keeps just the profile name and its project override when the profile's settings fail to load", async () => {
      fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
        Promise.resolve(registry(profile === "b")),
      );
      fetchSettings.mockImplementation((profile?: string) =>
        profile === "b" ? Promise.reject(new Error("settings down")) : Promise.resolve({}),
      );
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));

      await pickProfile(findByText, /^b/);
      await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("b"));
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ profile: "b", sandbox: true });
    });

    it("switches profile despite a failed registry lookup when no project is selected", async () => {
      fetchProjects.mockResolvedValue(null);
      const { getByText, findByText } = renderWizard({ scratch: true });
      await pickProfile(findByText, /^b/);
      await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("b"));
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ profile: "b", scratch: true });
    });

    it("still applies the mount-time registry response after a switch to another profile failed", async () => {
      const initial = deferred();
      fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
        profile === "b" ? Promise.resolve(null) : initial.promise,
      );
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));

      await pickProfile(findByText, /^b/);
      await findByText(/Could not load project settings for profile b/);
      await act(async () => initial.resolve(registry(true)));
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      const body = createSession.mock.calls[0][0];
      expect(body).toMatchObject({ path: "/repo/alpha", sandbox: true });
      expect(body.profile).not.toBe("b");
    });

    it("resolves the overrides for the project selected when the switch commits", async () => {
      const b = deferred();
      const both = (sandbox: boolean) =>
        ["/repo/alpha", "/repo/beta"].map((path) => ({
          name: path.split("/").pop(),
          path,
          scope: "profile",
          pinned: false,
          overrides: { sandbox_enabled: sandbox },
        }));
      // alpha is remembered; beta is picked while b is still loading. Off in the served profile, on in b.
      fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
        profile === "b" ? b.promise : Promise.resolve(both(false)),
      );
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));

      await pickProfile(findByText, /^b/);
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, "b"));
      fireEvent.click(await findByText("Project"));
      fireEvent.click((await findByText("/repo/beta")).closest("button")!);
      await act(async () => b.resolve(both(true)));
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      expect(createSession.mock.calls[0][0]).toMatchObject({ path: "/repo/beta", profile: "b", sandbox: true });
    });

    it("rolls back to the last committed profile when a switch fails while an earlier one is still pending", async () => {
      const b = deferred();
      fetchProjects.mockImplementation((_scope?: string, profile?: string) =>
        profile === "b" ? b.promise : Promise.resolve(profile === "c" ? null : registry(false)),
      );
      fetchSettings.mockImplementation((profile?: string) =>
        Promise.resolve({ sandbox: { enabled_by_default: profile === "b" } }),
      );
      const { getByText, findByText } = renderWizard();
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, undefined));

      await pickProfile(findByText, /^b/);
      await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith(undefined, "b"));
      await pickProfile(findByText, /^c/);
      await findByText(/Could not load project settings for profile c/);
      // b answers late, after c already failed: it must not become the profile either.
      await act(async () => b.resolve(registry(true)));
      await clickLaunch(getByText);

      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
      const body = createSession.mock.calls[0][0];
      expect(body).toMatchObject({ sandbox: false });
      expect(body.profile).not.toBe("b");
      expect(body.profile).not.toBe("c");
    });
  });
});

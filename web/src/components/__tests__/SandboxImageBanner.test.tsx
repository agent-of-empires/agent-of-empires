// @vitest-environment jsdom
// Sandbox image banner (#4591): which state shows for a Daemon status, and the pull flow.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { SandboxImageStatus } from "../../lib/api";
import { useSandboxImageBanner } from "../../hooks/useSandboxImageBanner";
import { SandboxImageBanner } from "../SandboxImageBanner";

const fetchSandboxImageStatus = vi.fn();
const pullSandboxImage = vi.fn((): Promise<{ ok: boolean; message?: string }> => Promise.resolve({ ok: true }));
const dismissSandboxImage = vi.fn(() => Promise.resolve(true));
vi.mock("../../lib/api", () => ({
  fetchSandboxImageStatus: () => fetchSandboxImageStatus(),
  pullSandboxImage: () => pullSandboxImage(),
  dismissSandboxImage: (digest: string) => dismissSandboxImage(digest),
}));
const writeClipboard = vi.fn(() => Promise.resolve(true));
vi.mock("../../lib/clipboard", () => ({ writeClipboard: (text: string) => writeClipboard(text) }));

const IMAGE = "ghcr.io/agent-of-empires/aoe-sandbox:latest";

function status(overrides: Partial<SandboxImageStatus> = {}): SandboxImageStatus {
  return {
    image: IMAGE,
    state: "available",
    remote_digest: "sha256:new",
    dismissed: false,
    checks_enabled: true,
    sandbox_in_use: true,
    pull: { state: "idle" },
    pull_blocked: null,
    pull_command: `docker pull ${IMAGE}`,
    runtime: "Docker",
    ...overrides,
  } as SandboxImageStatus;
}

function Harness() {
  const view = useSandboxImageBanner();
  return view ? <SandboxImageBanner view={view} /> : null;
}

async function mount(first: SandboxImageStatus | { error: string }) {
  fetchSandboxImageStatus.mockResolvedValue(first);
  const utils = render(<Harness />);
  await act(() => vi.advanceTimersByTimeAsync(0));
  return utils;
}

const banner = () => screen.queryByRole("status");
const pollAfter = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));

beforeEach(() => {
  vi.useFakeTimers();
  fetchSandboxImageStatus.mockReset();
  pullSandboxImage.mockClear();
  dismissSandboxImage.mockClear();
  writeClipboard.mockClear();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("SandboxImageBanner", () => {
  it("shows only an undismissed available update, and only when checks are on and a Sandbox is in use", async () => {
    const cases: [string, Partial<SandboxImageStatus>, string | null][] = [
      ["available", {}, "Sandbox image update available"],
      ["no Sandbox in use", { sandbox_in_use: false }, null],
      ["update checks off", { checks_enabled: false }, null],
      ["dismissed for this digest", { dismissed: true }, null],
      ["current", { state: "current" } as Partial<SandboxImageStatus>, null],
      // The Daemon can't tell, so the banner says nothing rather than "up to date".
      ["unknown", { state: "unknown", reason: "pinned" } as Partial<SandboxImageStatus>, null],
      // Docker Desktop not started: not "not pulled", and nothing to offer.
      ["runtime unavailable", { state: "unknown", reason: "runtime_unavailable" } as Partial<SandboxImageStatus>, null],
      ["check failed", { state: "check_failed", error: "timeout" } as Partial<SandboxImageStatus>, null],
      // The Daemon keeps its last pull; one this page didn't watch isn't news.
      ["a stale finished pull", { pull: { state: "done" } }, "Sandbox image update available"],
    ];
    for (const [label, overrides, name] of cases) {
      const { unmount } = await mount(status(overrides));
      expect(banner()?.getAttribute("aria-label") ?? null, label).toBe(name);
      unmount();
    }
  });

  it("says the Daemon can't read its configuration instead of going quiet, and stays quiet for other refusals", async () => {
    const cases: [string, string | null][] = [
      ["config_unreadable", "AoE can't read its configuration"],
      ["http_502", null],
    ];
    for (const [error, name] of cases) {
      const { unmount } = await mount({ error });
      expect(banner()?.getAttribute("aria-label") ?? null, error).toBe(name);
      if (name) {
        expect(banner()?.textContent).toContain("can't read its configuration");
        fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
        expect(banner()).toBeNull();
      }
      unmount();
    }
  });

  it("offers the configured runtime's pull command when the dashboard can't pull", async () => {
    const commands = [`docker pull ${IMAGE}`, `podman pull ${IMAGE}`, `container image pull ${IMAGE}`];
    for (const [i, pull_command] of commands.entries()) {
      const blocked = (["disabled", "read_only", "cityhall"] as const)[i];
      const { unmount } = await mount(status({ pull_blocked: blocked, pull_command }));
      expect(screen.getByText(pull_command)).toBeTruthy();
      expect(screen.queryByRole("button", { name: "Update now" }), blocked).toBeNull();
      fireEvent.click(screen.getByRole("button", { name: `Copy ${pull_command}` }));
      await act(() => vi.advanceTimersByTimeAsync(0));
      expect(writeClipboard).toHaveBeenLastCalledWith(pull_command);
      expect(screen.getByText("Copied")).toBeTruthy();
      unmount();
    }
  });

  it("pulls, shows progress, then the outcome, and polls fast only while pulling", async () => {
    await mount(status());
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    expect(pullSandboxImage).toHaveBeenCalledTimes(1);
    expect(banner()?.getAttribute("aria-label")).toBe("Downloading sandbox image");
    expect(screen.queryByRole("button", { name: /Dismiss/ })).toBeNull();

    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    await pollAfter(0);
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    const calls = fetchSandboxImageStatus.mock.calls.length;
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(calls + 1);
    expect(banner()?.textContent).toContain("running sessions keep the image they started with");

    // Back to the hourly cadence once the pull is over.
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(calls + 1);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
    expect(banner()).toBeNull();
  });

  it("reports a failed pull with the runtime's stderr and retries", async () => {
    await mount(status());
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "failed", error: "denied: requested access" } }));
    await pollAfter(0);
    expect(banner()?.getAttribute("aria-label")).toBe("Couldn't download the sandbox image");

    const details = screen.getByRole("button", { name: "Details" });
    const stderr = screen.getByText("denied: requested access");
    expect(stderr.closest("[hidden]")).not.toBeNull();
    fireEvent.click(details);
    expect(details.getAttribute("aria-expanded")).toBe("true");
    expect(stderr.closest("[hidden]")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(pullSandboxImage).toHaveBeenCalledTimes(2);
    expect(banner()?.getAttribute("aria-label")).toBe("Downloading sandbox image");
  });

  it("shows a refused pull as refused, never as an older pull's success", async () => {
    // The Daemon still holds an earlier pull's "done" while the new request is refused.
    await mount(status({ pull: { state: "done" } }));
    pullSandboxImage.mockResolvedValueOnce({ ok: false, message: "Pulling sandbox images from the web is off." });
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    await pollAfter(0);
    expect(banner()?.getAttribute("aria-label")).toBe("Couldn't download the sandbox image");
    expect(screen.getByText("Pulling sandbox images from the web is off.")).toBeTruthy();

    // A failing status request after the refusal doesn't bring back "Downloading".
    fetchSandboxImageStatus.mockResolvedValue({ error: "http_500" });
    await pollAfter(0);
    expect(banner()?.getAttribute("aria-label")).toBe("Couldn't download the sandbox image");

    // No reply at all says the Daemon wasn't reached, not that it refused.
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "done" } }));
    pullSandboxImage.mockResolvedValueOnce({ ok: false });
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    await pollAfter(0);
    expect(screen.getByText("Couldn't reach the Daemon.")).toBeTruthy();

    // Refused, and the status check after it fails too: dismissing the error must not leave this
    // page's own "Downloading" guess standing in for a pull that never started.
    fetchSandboxImageStatus.mockResolvedValue({ error: "http_500" });
    pullSandboxImage.mockResolvedValueOnce({ ok: false, message: "Pulling sandbox images from the web is off." });
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    await pollAfter(0);
    expect(banner()?.getAttribute("aria-label")).toBe("Couldn't download the sandbox image");
    fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
    expect(banner()?.getAttribute("aria-label")).not.toBe("Downloading sandbox image");
  });

  it("keeps polling fast after an accepted pull even when the status request fails", async () => {
    await mount(status());
    fetchSandboxImageStatus.mockResolvedValue({ error: "http_500" });
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    await pollAfter(0);
    const calls = fetchSandboxImageStatus.mock.calls.length;
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(calls + 1);
    // Also once a status has shown the pull running and a later request fails.
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    await pollAfter(2000);
    fetchSandboxImageStatus.mockResolvedValue({ error: "http_500" });
    await pollAfter(2000);
    const afterFailure = fetchSandboxImageStatus.mock.calls.length;
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(afterFailure + 1);
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(2000);
    expect(banner()?.getAttribute("aria-label")).toBe("Sandbox image updated");
  });

  it("follows the Daemon's pull when the Update now reply is lost", async () => {
    await mount(status());
    // A network failure: no reply, so no refusal message either.
    pullSandboxImage.mockResolvedValueOnce({ ok: false });
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    await pollAfter(0);
    expect(banner()?.getAttribute("aria-label")).toBe("Downloading sandbox image");
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(2000);
    expect(banner()?.getAttribute("aria-label")).toBe("Sandbox image updated");
  });

  it("keeps a pull a status confirmed when its Update now reply fails later", async () => {
    await mount(status());
    let reply: (value: { ok: boolean; message?: string }) => void = () => {};
    pullSandboxImage.mockImplementationOnce(() => new Promise((resolve) => (reply = resolve)));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    expect(pullSandboxImage).toHaveBeenCalledTimes(1);
    // A status poll lands while the reply is still out and sees the pull running.
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(banner()?.getAttribute("aria-label")).toBe("Downloading sandbox image");
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await act(async () => {
      reply({ ok: false });
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(banner()?.getAttribute("aria-label")).toBe("Sandbox image updated");
  });

  it("keeps showing the pull it asked for when an older status lands before the reply", async () => {
    await mount(status());
    let reply: (value: { ok: boolean; message?: string }) => void = () => {};
    pullSandboxImage.mockImplementationOnce(() => new Promise((resolve) => (reply = resolve)));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    // A poll from before the pull started still says idle.
    fetchSandboxImageStatus.mockResolvedValue(status());
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(banner()?.getAttribute("aria-label")).toBe("Downloading sandbox image");
    // And polling stays fast.
    const calls = fetchSandboxImageStatus.mock.calls.length;
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(calls + 1);
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await act(async () => {
      reply({ ok: true });
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(banner()?.getAttribute("aria-label")).toBe("Sandbox image updated");
  });

  it("doesn't let a poll sent before Update now overwrite the pull it asked for", async () => {
    // (the older poll's pull state): done would claim success for the new request, idle would
    // bring the offer back, failed would report the old failure.
    const older: SandboxImageStatus["pull"][] = [
      { state: "done" },
      { state: "idle" },
      { state: "failed", error: "old failure" },
    ];
    for (const pull of older) {
      const { unmount } = await mount(status());
      // A poll leaves before the click and its answer is held.
      let answerOld: (value: SandboxImageStatus) => void = () => {};
      fetchSandboxImageStatus.mockImplementationOnce(() => new Promise((resolve) => (answerOld = resolve)));
      await act(async () => {
        document.dispatchEvent(new Event("visibilitychange"));
        await vi.advanceTimersByTimeAsync(0);
      });
      // Update now, with its reply held too.
      let reply: (value: { ok: boolean; message?: string }) => void = () => {};
      pullSandboxImage.mockImplementationOnce(() => new Promise((resolve) => (reply = resolve)));
      fireEvent.click(screen.getByRole("button", { name: "Update now" }));
      fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
      await act(async () => {
        answerOld(status({ pull }));
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(banner()?.getAttribute("aria-label"), pull.state).toBe("Downloading sandbox image");
      const calls = fetchSandboxImageStatus.mock.calls.length;
      await pollAfter(2000);
      expect(fetchSandboxImageStatus.mock.calls.length, `${pull.state}: polls fast`).toBe(calls + 1);
      // The confirmed pull's own outcome then shows at once.
      fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
      await act(async () => {
        reply({ ok: true });
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(banner()?.getAttribute("aria-label"), pull.state).toBe("Sandbox image updated");
      unmount();
    }
  });

  it("polls fast from Update now even while its reply is held", async () => {
    await mount(status());
    pullSandboxImage.mockImplementationOnce(() => new Promise(() => {}));
    const before = fetchSandboxImageStatus.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    // No visibility change, reconnect or reply: the click alone starts the fast cadence.
    await pollAfter(0);
    await pollAfter(2000);
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(before + 3);
    expect(banner()?.getAttribute("aria-label")).toBe("Downloading sandbox image");
  });

  it("ignores a reply from a pull attempt that Retry superseded", async () => {
    await mount(status());
    type Reply = { ok: boolean; message?: string };
    const replies: ((r: Reply) => void)[] = [];
    pullSandboxImage.mockImplementation(() => new Promise<Reply>((resolve) => replies.push(resolve)));
    const label = () => banner()?.getAttribute("aria-label");
    // Attempt A runs and fails while its reply is still held.
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    await pollAfter(0);
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "failed", error: "A failed" } }));
    await pollAfter(2000);
    expect(label()).toBe("Couldn't download the sandbox image");

    // Attempt B: A's late refusal must neither clear B's progress nor show A's message.
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(label()).toBe("Downloading sandbox image");
    await act(async () => replies[0]!({ ok: false, message: "A was refused" }));
    await pollAfter(0);
    expect(label()).toBe("Downloading sandbox image");
    expect(banner()?.textContent).not.toContain("A was refused");

    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    await pollAfter(2000);
    await act(async () => replies[1]!({ ok: true }));
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(2000);
    expect(label()).toBe("Sandbox image updated");
    pullSandboxImage.mockImplementation(() => Promise.resolve({ ok: true }));
  });

  it("after a watched pull, offers an image published since, but not the one it pulled", async () => {
    // (status once the pull is done, banner)
    const cases: [string, Partial<SandboxImageStatus>, string][] = [
      ["now current", { state: "current" } as Partial<SandboxImageStatus>, "Sandbox image updated"],
      ["the pulled digest still offered", {}, "Sandbox image updated"],
      ["a newer digest", { remote_digest: "sha256:newer" }, "Sandbox image update available"],
    ];
    for (const [name, after, want] of cases) {
      const { unmount } = await mount(status());
      fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
      fireEvent.click(screen.getByRole("button", { name: "Update now" }));
      await pollAfter(0);
      fetchSandboxImageStatus.mockResolvedValue(status({ ...after, pull: { state: "done" } }));
      await pollAfter(2000);
      expect(banner()?.getAttribute("aria-label"), name).toBe(want);
      unmount();
    }
  });

  it("stops standing in for an unseen pull when the Update now reply never comes", async () => {
    await mount(status());
    // The reply never settles, and no status ever catches the pull running.
    pullSandboxImage.mockImplementationOnce(() => new Promise(() => {}));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(0);
    });
    await pollAfter(20_000);
    expect(banner()?.getAttribute("aria-label")).toBe("Downloading sandbox image");
    // Past the cap, the Daemon's own status is believed: the pull finished.
    await pollAfter(12_000);
    expect(banner()?.getAttribute("aria-label")).toBe("Sandbox image updated");
  });

  it("keeps watching for the pull past the cap while the Update now reply is out", async () => {
    await mount(status());
    pullSandboxImage.mockImplementationOnce(() => new Promise(() => {}));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    // No status shows it yet, and no visibility or reconnect event comes.
    await pollAfter(32_000);
    const calls = fetchSandboxImageStatus.mock.calls.length;
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(calls + 1);
    // A delayed pull starts, and is followed to its outcome.
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    await pollAfter(2000);
    expect(banner()?.getAttribute("aria-label")).toBe("Downloading sandbox image");
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(2000);
    expect(banner()?.getAttribute("aria-label")).toBe("Sandbox image updated");
  });

  it("shows a refusal that arrives after an older outcome was dismissed", async () => {
    await mount(status());
    let reply!: (r: { ok: boolean; message?: string }) => void;
    pullSandboxImage.mockImplementationOnce(() => new Promise((resolve) => (reply = resolve)));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    // Past the cap an older finished pull shows, and the user dismisses it.
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(32_000);
    expect(banner()?.getAttribute("aria-label")).toBe("Sandbox image updated");
    fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
    expect(banner()).toBeNull();
    // Then the Daemon refuses this click.
    await act(async () => reply({ ok: false, message: "Pulling is off on this server." }));
    expect(banner()?.getAttribute("aria-label")).toBe("Couldn't download the sandbox image");
    expect(banner()?.textContent).toContain("Pulling is off on this server.");
    expect(screen.getByRole("button", { name: "Retry" })).toBeTruthy();
  });

  it("ties an outcome to this click only once a status or the accepted reply does", async () => {
    // (reply, outcome status after the cap, banner)
    const cases: [string, { ok: boolean } | null, Partial<SandboxImageStatus>, string][] = [
      // No status saw it running and no reply came: an older pull's outcome isn't this click's.
      ["older done", null, { pull: { state: "done" } }, "Sandbox image update available"],
      ["older failed", null, { pull: { state: "failed", error: "old" } }, "Sandbox image update available"],
      // The Daemon claims the pull before it answers, so after an accepted reply it is this click's.
      ["accepted, then done", { ok: true }, { pull: { state: "done" } }, "Sandbox image updated"],
    ];
    for (const [name, reply, after, want] of cases) {
      const { unmount } = await mount(status());
      pullSandboxImage.mockImplementationOnce(() => (reply ? Promise.resolve(reply) : new Promise(() => {})));
      fireEvent.click(screen.getByRole("button", { name: "Update now" }));
      fetchSandboxImageStatus.mockResolvedValue(status(after));
      await pollAfter(32_000);
      expect(banner()?.getAttribute("aria-label"), name).toBe(want);
      unmount();
    }
  });

  it("hides an earlier available update once a check fails", async () => {
    await mount(status());
    expect(banner()).not.toBeNull();
    fetchSandboxImageStatus.mockResolvedValue({ ...status(), state: "check_failed", error: "registry timed out" });
    await pollAfter(60 * 60_000);
    expect(banner()).toBeNull();
  });

  it("keeps text at text contrast on the tints, with the state colour on the dot", async () => {
    await mount(status());
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(0);
    const done = banner() as HTMLElement;
    expect(done.className).toContain("text-text-primary");
    expect(done.className).not.toContain("text-status-running");

    cleanup();
    await mount({ error: "config_unreadable" });
    const failed = banner() as HTMLElement;
    expect(failed.className).toContain("text-text-primary");
    expect(failed.className).not.toMatch(/\btext-status-error/);
    // Secondary text and the close button use the token lifted for small text on these tints.
    cleanup();
    await mount(status());
    expect(screen.getByText(`· ${IMAGE}`).className).toContain("text-text-subtle");
    expect(screen.getByRole("button", { name: "Dismiss sandbox image notice" }).className).toContain(
      "text-text-subtle",
    );
  });

  it("explains why on request and dismisses by the remote digest", async () => {
    await mount(status());
    const why = screen.getByRole("button", { name: "Why?" });
    const text = screen.getByText(/doesn't update automatically/);
    expect(text.closest("[hidden]")).not.toBeNull();
    fireEvent.click(why);
    expect(text.closest("[hidden]")).toBeNull();
    fireEvent.keyDown(why, { key: "Escape" });
    expect(text.closest("[hidden]")).not.toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
    expect(dismissSandboxImage).toHaveBeenCalledWith("sha256:new");
    expect(banner()).toBeNull();
  });

  it("keeps a dismissed notice hidden even when the Daemon can't save the dismissal", async () => {
    // Read-only and CityHall servers always refuse the save; elsewhere it can fail in transit.
    for (const pull_blocked of [null, "read_only", "cityhall"] as const) {
      dismissSandboxImage.mockResolvedValueOnce(false);
      const { unmount } = await mount(status({ pull_blocked }));
      fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
      expect(dismissSandboxImage).toHaveBeenLastCalledWith("sha256:new");
      // The next hourly status still says it isn't dismissed.
      await pollAfter(60 * 60_000);
      expect(banner(), String(pull_blocked)).toBeNull();
      unmount();
    }
  });
});

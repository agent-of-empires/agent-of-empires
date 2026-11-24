// @vitest-environment jsdom
// Sandbox image banner (#4591): which state shows for a Daemon status, and the pull flow.

import { afterEach, beforeEach, describe, expect, it, onTestFinished, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { SandboxImagePull, SandboxImageStatus } from "../../lib/api";
import { useSandboxImageBanner } from "../../hooks/useSandboxImageBanner";
import { notifySandboxImagePullStarted, notifySandboxImagePulled } from "../../lib/sandboxImageEvents";
import { CANCEL_NOT_READY, SandboxImageBanner } from "../SandboxImageBanner";

const fetchSandboxImageStatus = vi.fn();
const pullSandboxImage = vi.fn((): Promise<{ ok: boolean; message?: string }> => Promise.resolve({ ok: true }));
const dismissSandboxImage = vi.fn(() => Promise.resolve(true));
const cancelSandboxImagePull = vi.fn(
  (_image: string): Promise<{ ok: boolean; pull?: SandboxImagePull; message?: string }> =>
    Promise.resolve({ ok: true, pull: { state: "cancelled" } }),
);
vi.mock("../../lib/api", () => ({
  fetchSandboxImageStatus: () => fetchSandboxImageStatus(),
  pullSandboxImage: () => pullSandboxImage(),
  dismissSandboxImage: (digest: string) => dismissSandboxImage(digest),
  cancelSandboxImagePull: (image: string) => cancelSandboxImagePull(image),
}));
const writeClipboard = vi.fn(() => Promise.resolve(true));
vi.mock("../../lib/clipboard", () => ({ writeClipboard: (text: string) => writeClipboard(text) }));

const IMAGE = "ghcr.io/agent-of-empires/aoe-sandbox:latest";
const DOWNLOADING = "Downloading sandbox image…";
const UPDATED = "Sandbox image updated. Applies to new Sessions.";
const CANCELLED = "Update cancelled. Sessions keep the current image.";
const FAILED = "Couldn't download the sandbox image.";

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
  const { view, running } = useSandboxImageBanner();
  if (running?.hidden) {
    return (
      <button type="button" onClick={running.onShow}>
        Show pull
      </button>
    );
  }
  return view ? <SandboxImageBanner view={view} /> : null;
}

async function mount(first: SandboxImageStatus | { error: string }) {
  fetchSandboxImageStatus.mockResolvedValue(first);
  const utils = render(<Harness />);
  await act(() => vi.advanceTimersByTimeAsync(0));
  return utils;
}

/** The live part: the state's words, read as a screen reader announces them. */
const banner = () => screen.queryByRole("status");
/** The whole banner, with its controls and disclosures. */
const bar = () => screen.queryByTestId("sandbox-image-banner");
const pollAfter = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));

beforeEach(() => {
  vi.useFakeTimers();
  fetchSandboxImageStatus.mockReset();
  pullSandboxImage.mockClear();
  dismissSandboxImage.mockClear();
  cancelSandboxImagePull.mockClear();
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
      expect(banner()?.textContent ?? null, label).toBe(name);
      unmount();
    }
  });

  it("says the Daemon can't read its configuration instead of going quiet, and stays quiet for other refusals", async () => {
    const cases: [string, string | null][] = [
      [
        "config_unreadable",
        "AoE can't read its configuration (config.toml or state.toml), so sandbox image updates aren't checked. Fix the file, then reload.",
      ],
      ["http_502", null],
    ];
    for (const [error, name] of cases) {
      const { unmount } = await mount({ error });
      expect(banner()?.textContent ?? null, error).toBe(name);
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
      // The banner's own status holds only the phase, so the feedback needs a live region of its own.
      expect(screen.getAllByRole("status").map((el) => el.textContent)).toContain("Copied");
      unmount();
    }
  });

  it("pulls, shows progress, then the outcome, and polls fast only while pulling", async () => {
    await mount(status());
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    expect(pullSandboxImage).toHaveBeenCalledTimes(1);
    expect(banner()?.textContent).toBe(DOWNLOADING);
    expect(screen.queryByRole("button", { name: /Dismiss/ })).toBeNull();

    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    await pollAfter(0);
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    const calls = fetchSandboxImageStatus.mock.calls.length;
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(calls + 1);
    expect(banner()?.textContent).toBe(UPDATED);

    // Back to the hourly cadence once the pull is over.
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(calls + 1);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
    expect(banner()).toBeNull();
  });

  it("reports a failed pull with the runtime's stderr and retries", async () => {
    await mount(status());
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    fetchSandboxImageStatus.mockResolvedValue(
      status({ pull: { state: "failed", reason: "other", message: FAILED, error: "denied: requested access" } }),
    );
    await pollAfter(0);
    expect(banner()?.textContent).toBe(FAILED);

    const details = screen.getByRole("button", { name: "Details" });
    const stderr = screen.getByText("denied: requested access");
    expect(stderr.closest("[hidden]")).not.toBeNull();
    fireEvent.click(details);
    expect(details.getAttribute("aria-expanded")).toBe("true");
    expect(stderr.closest("[hidden]")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(pullSandboxImage).toHaveBeenCalledTimes(2);
    expect(banner()?.textContent).toBe(DOWNLOADING);
  });

  it("shows a refused pull as refused, never as an older pull's success", async () => {
    // The Daemon still holds an earlier pull's "done" while the new request is refused.
    await mount(status({ pull: { state: "done" } }));
    pullSandboxImage.mockResolvedValueOnce({ ok: false, message: "Pulling sandbox images from the web is off." });
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    await pollAfter(0);
    expect(banner()?.textContent).toBe(FAILED);
    expect(screen.getByText("Pulling sandbox images from the web is off.")).toBeTruthy();

    // A failing status request after the refusal doesn't bring back "Downloading".
    fetchSandboxImageStatus.mockResolvedValue({ error: "http_500" });
    await pollAfter(0);
    expect(banner()?.textContent).toBe(FAILED);

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
    expect(banner()?.textContent).toBe(FAILED);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
    expect(banner()?.textContent).not.toBe(DOWNLOADING);
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
    expect(banner()?.textContent).toBe(UPDATED);
  });

  it("follows the Daemon's pull when the Update now reply is lost", async () => {
    await mount(status());
    // A network failure: no reply, so no refusal message either.
    pullSandboxImage.mockResolvedValueOnce({ ok: false });
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    await pollAfter(0);
    expect(banner()?.textContent).toBe(DOWNLOADING);
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(2000);
    expect(banner()?.textContent).toBe(UPDATED);
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
    expect(banner()?.textContent).toBe(DOWNLOADING);
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await act(async () => {
      reply({ ok: false });
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(banner()?.textContent).toBe(UPDATED);
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
    expect(banner()?.textContent).toBe(DOWNLOADING);
    // And polling stays fast.
    const calls = fetchSandboxImageStatus.mock.calls.length;
    await pollAfter(2000);
    expect(fetchSandboxImageStatus.mock.calls.length).toBe(calls + 1);
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await act(async () => {
      reply({ ok: true });
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(banner()?.textContent).toBe(UPDATED);
  });

  it("doesn't let a poll sent before Update now overwrite the pull it asked for", async () => {
    // (the older poll's pull state): done would claim success for the new request, idle would
    // bring the offer back, failed would report the old failure.
    const older: SandboxImageStatus["pull"][] = [
      { state: "done" },
      { state: "idle" },
      { state: "failed", reason: "other", message: FAILED, error: "old failure" },
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
      expect(banner()?.textContent, pull.state).toBe(DOWNLOADING);
      const calls = fetchSandboxImageStatus.mock.calls.length;
      await pollAfter(2000);
      expect(fetchSandboxImageStatus.mock.calls.length, `${pull.state}: polls fast`).toBe(calls + 1);
      // The confirmed pull's own outcome then shows at once.
      fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
      await act(async () => {
        reply({ ok: true });
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(banner()?.textContent, pull.state).toBe(UPDATED);
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
    expect(banner()?.textContent).toBe(DOWNLOADING);
  });

  it("ignores a reply from a pull attempt that Retry superseded", async () => {
    await mount(status());
    type Reply = { ok: boolean; message?: string };
    const replies: ((r: Reply) => void)[] = [];
    pullSandboxImage.mockImplementation(() => new Promise<Reply>((resolve) => replies.push(resolve)));
    const label = () => banner()?.textContent;
    // Attempt A runs and fails while its reply is still held.
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    await pollAfter(0);
    fetchSandboxImageStatus.mockResolvedValue(
      status({ pull: { state: "failed", reason: "other", message: FAILED, error: "A failed" } }),
    );
    await pollAfter(2000);
    expect(label()).toBe(FAILED);

    // Attempt B: A's late refusal must neither clear B's progress nor show A's message.
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(label()).toBe(DOWNLOADING);
    await act(async () => replies[0]!({ ok: false, message: "A was refused" }));
    await pollAfter(0);
    expect(label()).toBe(DOWNLOADING);
    expect(bar()?.textContent).not.toContain("A was refused");

    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
    await pollAfter(2000);
    await act(async () => replies[1]!({ ok: true }));
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(2000);
    expect(label()).toBe(UPDATED);
    pullSandboxImage.mockImplementation(() => Promise.resolve({ ok: true }));
  });

  it("after a watched pull, offers an image published since, but not the one it pulled", async () => {
    // (status once the pull is done, banner)
    const cases: [string, Partial<SandboxImageStatus>, string][] = [
      ["now current", { state: "current" } as Partial<SandboxImageStatus>, UPDATED],
      ["the pulled digest still offered", {}, UPDATED],
      ["a newer digest", { remote_digest: "sha256:newer" }, "Sandbox image update available"],
    ];
    for (const [name, after, want] of cases) {
      const { unmount } = await mount(status());
      fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "running" } }));
      fireEvent.click(screen.getByRole("button", { name: "Update now" }));
      await pollAfter(0);
      fetchSandboxImageStatus.mockResolvedValue(status({ ...after, pull: { state: "done" } }));
      await pollAfter(2000);
      expect(banner()?.textContent, name).toBe(want);
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
    expect(banner()?.textContent).toBe(DOWNLOADING);
    // Past the cap, the Daemon's own status is believed: the pull finished.
    await pollAfter(12_000);
    expect(banner()?.textContent).toBe(UPDATED);
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
    expect(banner()?.textContent).toBe(DOWNLOADING);
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(2000);
    expect(banner()?.textContent).toBe(UPDATED);
  });

  it("shows a refusal that arrives after an older outcome was dismissed", async () => {
    await mount(status());
    let reply!: (r: { ok: boolean; message?: string }) => void;
    pullSandboxImage.mockImplementationOnce(() => new Promise((resolve) => (reply = resolve)));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    // Past the cap an older finished pull shows, and the user dismisses it.
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(32_000);
    expect(banner()?.textContent).toBe(UPDATED);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
    expect(banner()).toBeNull();
    // Then the Daemon refuses this click.
    await act(async () => reply({ ok: false, message: "Pulling is off on this server." }));
    expect(banner()?.textContent).toBe(FAILED);
    expect(bar()?.textContent).toContain("Pulling is off on this server.");
    expect(screen.getByRole("button", { name: "Retry" })).toBeTruthy();
  });

  it("ties an outcome to this click only once a status or the accepted reply does", async () => {
    // (reply, outcome status after the cap, banner)
    const cases: [string, { ok: boolean } | null, Partial<SandboxImageStatus>, string][] = [
      // No status saw it running and no reply came: an older pull's outcome isn't this click's.
      ["older done", null, { pull: { state: "done" } }, "Sandbox image update available"],
      [
        "older failed",
        null,
        { pull: { state: "failed", reason: "other", message: FAILED, error: "old" } },
        "Sandbox image update available",
      ],
      // The Daemon claims the pull before it answers, so after an accepted reply it is this click's.
      ["accepted, then done", { ok: true }, { pull: { state: "done" } }, UPDATED],
    ];
    for (const [name, reply, after, want] of cases) {
      const { unmount } = await mount(status());
      pullSandboxImage.mockImplementationOnce(() => (reply ? Promise.resolve(reply) : new Promise(() => {})));
      fireEvent.click(screen.getByRole("button", { name: "Update now" }));
      fetchSandboxImageStatus.mockResolvedValue(status(after));
      await pollAfter(32_000);
      expect(banner()?.textContent, name).toBe(want);
      unmount();
    }
  });

  it("shows progress, not an older state already on screen, once the reply is accepted", async () => {
    const older: SandboxImageStatus["pull"][] = [
      { state: "done" },
      { state: "failed", reason: "other", message: FAILED, error: "old" },
      { state: "idle" },
    ];
    for (const pull of older) {
      const { unmount } = await mount(status());
      let reply!: (r: { ok: boolean }) => void;
      pullSandboxImage.mockImplementationOnce(() => new Promise((resolve) => (reply = resolve)));
      fireEvent.click(screen.getByRole("button", { name: "Update now" }));
      // Past the cap the state the Daemon still reports shows as the offer.
      fetchSandboxImageStatus.mockResolvedValue(status({ pull }));
      await pollAfter(32_000);
      expect(banner()?.textContent, pull.state).toBe("Sandbox image update available");
      // The Daemon accepts this click; the status fetched after that is still out.
      let fresh!: (s: SandboxImageStatus) => void;
      fetchSandboxImageStatus.mockImplementation(() => new Promise((resolve) => (fresh = resolve)));
      await act(async () => reply({ ok: true }));
      expect(banner()?.textContent, pull.state).toBe(DOWNLOADING);
      // The fresh status shows this pull's outcome.
      await act(async () => fresh(status({ state: "current", pull: { state: "done" } } as never)));
      expect(banner()?.textContent, pull.state).toBe(UPDATED);
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

  it("asks again as soon as a create pulled the image, not at the next hourly poll", async () => {
    for (const refused of [false, true]) {
      const { unmount } = await mount(status());
      if (refused) {
        pullSandboxImage.mockResolvedValueOnce({ ok: false, message: "Pulling is off on this server." });
        fireEvent.click(screen.getByRole("button", { name: "Update now" }));
        await pollAfter(0);
        expect(bar()?.textContent).toContain("Pulling is off on this server.");
      }
      expect(banner(), String(refused)).not.toBeNull();
      fetchSandboxImageStatus.mockResolvedValue(status({ state: "current" } as Partial<SandboxImageStatus>));
      act(() => notifySandboxImagePulled());
      await pollAfter(0);
      expect(banner(), String(refused)).toBeNull();
      unmount();
    }
  });

  it("ignores a late refusal of an Update now that a create's pull superseded", async () => {
    await mount(status());
    let reply!: (r: { ok: boolean; message?: string }) => void;
    pullSandboxImage.mockImplementationOnce(() => new Promise((resolve) => (reply = resolve)));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current" } as Partial<SandboxImageStatus>));
    act(() => notifySandboxImagePulled());
    await pollAfter(0);
    // Not "Downloading": the pending Update now no longer stands in for the status.
    expect(banner()).toBeNull();
    await act(async () => reply({ ok: false, message: "Pulling is off on this server." }));
    await pollAfter(0);
    expect(banner()).toBeNull();
  });

  it("keeps text at text contrast on the tints, with the state colour on the dot", async () => {
    await mount(status());
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } } as never));
    await pollAfter(0);
    const done = bar() as HTMLElement;
    expect(done.className).toContain("text-text-primary");
    expect(done.className).not.toContain("text-status-running");

    cleanup();
    await mount({ error: "config_unreadable" });
    const failed = bar() as HTMLElement;
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
    const text = screen.getByText(/Sandboxed Sessions run in the current image/);
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

describe("SandboxImageBanner progress, cancel and hide (#5751)", () => {
  const running = (pull: Partial<Extract<SandboxImagePull, { state: "running" }>> = {}) =>
    status({ state: "current", pull: { state: "running", ...pull } });

  it("announces only the phase, shows the layer count beside it, and draws the bar only once counts are known", async () => {
    // (pull, status words, visible count, bar width)
    const cases: [Partial<Extract<SandboxImagePull, { state: "running" }>>, string, string | null, string | null][] = [
      [{ phase: "downloading", layers_done: 3, layers_known: 7 }, DOWNLOADING, "3 of 7 layers", `${(3 / 7) * 100}%`],
      [{ phase: "unpacking", layers_done: 7, layers_known: 7 }, "Unpacking sandbox image…", "7 of 7 layers", "100%"],
      [{}, DOWNLOADING, null, null],
      // Zero layers known is no count yet, not a full or empty bar.
      [{ layers_done: 0, layers_known: 0 }, DOWNLOADING, null, null],
    ];
    for (const [pull, words, count, width] of cases) {
      const { unmount } = await mount(running(pull));
      const label = JSON.stringify(pull);
      expect(banner()?.textContent, label).toBe(words);
      const shown = count ? screen.getByText(count) : null;
      expect(shown?.getAttribute("aria-hidden") ?? null, label).toBe(count ? "true" : null);
      expect(bar()?.textContent?.includes("Sessions keep running"), label).toBe(true);
      const progress = screen.queryByTestId("sandbox-image-progress");
      expect(progress?.style.width ?? null, label).toBe(width);
      expect(progress?.getAttribute("aria-hidden") ?? null, label).toBe(width ? "true" : null);
      unmount();
    }
  });

  it("cancels in one click, then offers Update now from the cancelled outcome", async () => {
    let answer!: (r: { ok: boolean; pull?: SandboxImagePull }) => void;
    cancelSandboxImagePull.mockReturnValueOnce(new Promise((r) => (answer = r)));
    await mount(running({ layers_done: 1, layers_known: 7 }));
    const cancel = screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement;
    fireEvent.click(cancel);
    expect(cancelSandboxImagePull).toHaveBeenCalledOnce();
    expect(cancel.disabled).toBe(true);
    // The reply's state shows at once, before the status it triggers comes back.
    fetchSandboxImageStatus.mockReturnValueOnce(new Promise(() => {}));
    await act(async () => answer({ ok: true, pull: { state: "cancelled" } }));
    expect(banner()?.textContent).toBe("Update cancelled. Sessions keep the current image.");
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "available", pull: { state: "cancelled" } }));
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    expect(pullSandboxImage).toHaveBeenCalledOnce();
    expect(banner()?.textContent).toBe(DOWNLOADING);
  });

  it("says a refused Cancel couldn't cancel, with the Daemon's reason, until a click or the pull's end", async () => {
    const refused = `${DOWNLOADING} Couldn't cancel.`;
    const details = () => screen.queryByText("cancels are off here");
    cancelSandboxImagePull.mockResolvedValue({ ok: false, message: "cancels are off here" });
    onTestFinished(() =>
      cancelSandboxImagePull.mockReset().mockResolvedValue({ ok: true, pull: { state: "cancelled" } }),
    );
    await mount(running());
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await pollAfter(0);
    expect(banner()?.textContent).toBe(refused);
    fireEvent.click(screen.getByRole("button", { name: "Details" }));
    expect(details()?.closest("[hidden]")).toBeNull();
    expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(false);
    // Polls that still show the pull running keep the notice readable.
    await pollAfter(2000 * 3);
    expect(banner()?.textContent).toBe(refused);

    // Trying again clears it while the new request is out.
    let answer!: (r: { ok: boolean; message?: string }) => void;
    cancelSandboxImagePull.mockReturnValueOnce(new Promise((r) => (answer = r)));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(banner()?.textContent).toBe(DOWNLOADING);
    expect(details()).toBeNull();
    await act(async () => answer({ ok: false }));
    // No message from the Daemon: the reply never arrived.
    expect(banner()?.textContent).toBe(refused);
    fireEvent.click(screen.getByRole("button", { name: "Details" }));
    expect(screen.getByText("Couldn't reach the Daemon.")).toBeTruthy();

    // × clears it, so showing the pull again brings back the plain banner.
    fireEvent.click(screen.getByRole("button", { name: "Hide sandbox image notice" }));
    fireEvent.click(screen.getByRole("button", { name: "Show pull" }));
    expect(banner()?.textContent).toBe(DOWNLOADING);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await pollAfter(0);
    expect(banner()?.textContent).toBe(refused);
    // The pull ending clears it, so the next pull, started elsewhere, doesn't carry it.
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } }));
    await pollAfter(2000);
    expect(banner()?.textContent).toBe(UPDATED);
    fetchSandboxImageStatus.mockResolvedValue(running());
    await pollAfter(30_000);
    expect(banner()?.textContent).toBe(DOWNLOADING);
  });

  it("cancels the image on the banner, not whatever the default is by then", async () => {
    await mount({ ...running(), image: "ghcr.io/example/other:1" });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(cancelSandboxImagePull).toHaveBeenCalledWith("ghcr.io/example/other:1");
  });

  it("drops a Cancel once the banner shows another image, so its reply and refusal stay off it", async () => {
    const OTHER = "ghcr.io/example/other:1";
    const cancelButton = () => screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement;
    let answer!: (r: { ok: boolean; pull?: SandboxImagePull; message?: string }) => void;
    cancelSandboxImagePull.mockReturnValueOnce(new Promise((r) => (answer = r)));
    await mount(running());
    fireEvent.click(cancelButton());
    expect(cancelButton().disabled).toBe(true);
    // A poll shows another image's pull while this Cancel's reply is still out.
    fetchSandboxImageStatus.mockResolvedValue({ ...running(), image: OTHER });
    await pollAfter(2000);
    expect(cancelButton().disabled, "Cancelling… stayed on the other image").toBe(false);
    // Hold the poll the reply triggers, so only the reply itself can change what shows.
    fetchSandboxImageStatus.mockReturnValueOnce(new Promise(() => {}));
    await act(async () => answer({ ok: true, pull: { state: "cancelled" } }));
    // A's late reply leaves B running: no outcome, and Cancel still free for B.
    expect(banner()?.textContent).toBe(DOWNLOADING);
    expect(screen.queryByText(CANCELLED)).toBeNull();
    expect(cancelButton().disabled).toBe(false);
    fetchSandboxImageStatus.mockResolvedValue({ ...running(), image: OTHER });
    await pollAfter(2000);

    // A refused Cancel's notice goes too once another image is on the banner.
    cancelSandboxImagePull.mockResolvedValueOnce({ ok: false, message: "cancels are off here" });
    fireEvent.click(cancelButton());
    expect(cancelSandboxImagePull).toHaveBeenLastCalledWith(OTHER);
    await pollAfter(0);
    expect(banner()?.textContent).toBe(`${DOWNLOADING} Couldn't cancel.`);
    fetchSandboxImageStatus.mockResolvedValue(running());
    await pollAfter(2000);
    expect(banner()?.textContent).toBe(DOWNLOADING);
    expect(screen.queryByText("cancels are off here")).toBeNull();
  });

  it("ignores an earlier Cancel's reply once a later Cancel for the same image is out", async () => {
    const cancelButton = () => screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement;
    let first!: (r: { ok: boolean; pull?: SandboxImagePull; message?: string }) => void;
    let second!: (r: { ok: boolean; pull?: SandboxImagePull; message?: string }) => void;
    cancelSandboxImagePull
      .mockReturnValueOnce(new Promise((r) => (first = r)))
      .mockReturnValueOnce(new Promise((r) => (second = r)));
    await mount(running());
    fireEvent.click(cancelButton());
    // The banner moves to another image and back, so the first Cancel no longer holds it.
    fetchSandboxImageStatus.mockResolvedValue({ ...running(), image: "ghcr.io/example/other:1" });
    await pollAfter(2000);
    fetchSandboxImageStatus.mockResolvedValue(running());
    await pollAfter(2000);
    expect(cancelButton().disabled).toBe(false);
    fireEvent.click(cancelButton());
    expect(cancelButton().disabled).toBe(true);

    // The first reply, refused, lands while the second Cancel is out.
    fetchSandboxImageStatus.mockReturnValueOnce(new Promise(() => {}));
    await act(async () => first({ ok: false, message: "first refusal" }));
    expect(cancelButton().disabled, "the old reply freed the pending Cancel").toBe(true);
    expect(banner()?.textContent).toBe(DOWNLOADING);
    expect(screen.queryByText("first refusal")).toBeNull();

    // The second Cancel's own reply decides.
    await act(async () => second({ ok: true, pull: { state: "cancelled" } }));
    expect(banner()?.textContent).toBe(CANCELLED);
  });

  it("frees Cancel when a create's pull supersedes the attempt it was sent for", async () => {
    let answer!: (r: { ok: boolean; pull?: SandboxImagePull }) => void;
    cancelSandboxImagePull.mockReturnValueOnce(new Promise((r) => (answer = r)));
    await mount(running());
    const cancelButton = () => screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement;
    fireEvent.click(cancelButton());
    expect(cancelButton().disabled).toBe(true);
    act(() => notifySandboxImagePulled());
    await pollAfter(0);
    // The pull ends, and the next one is started elsewhere while the old Cancel reply is still out.
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } }));
    await pollAfter(2000);
    fetchSandboxImageStatus.mockResolvedValue(running());
    await pollAfter(30_000);
    expect(banner()?.textContent).toBe(DOWNLOADING);
    expect(cancelButton().disabled, "the superseded Cancel kept the button off").toBe(false);
    fetchSandboxImageStatus.mockReturnValueOnce(new Promise(() => {}));
    await act(async () => answer({ ok: true, pull: { state: "cancelled" } }));
    expect(banner()?.textContent, "the old reply ended the new pull").toBe(DOWNLOADING);
    fireEvent.click(cancelButton());
    expect(cancelSandboxImagePull).toHaveBeenCalledTimes(2);
  });

  it("ignores a Cancel reply that arrives after a newer update started", async () => {
    let answer!: (r: { ok: boolean; pull?: SandboxImagePull }) => void;
    cancelSandboxImagePull.mockReturnValueOnce(new Promise((r) => (answer = r)));
    await mount(running());
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    // A poll sees the cancel land before its reply does, and the user starts another update.
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "cancelled" } }));
    await pollAfter(2000);
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    fetchSandboxImageStatus.mockResolvedValue(running());
    await pollAfter(0);
    expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(false);
    // Hold the next poll, so what shows is the old reply's doing alone.
    fetchSandboxImageStatus.mockReturnValueOnce(new Promise(() => {}));
    await act(async () => answer({ ok: true, pull: { state: "cancelled" } }));
    expect(banner()?.textContent).toBe(DOWNLOADING);
  });

  it("keeps a cancelled outcome once Cancel answers, while the Update now reply is still out", async () => {
    pullSandboxImage.mockReturnValueOnce(new Promise(() => {}));
    await mount(status());
    // A status confirms the pull before its reply does.
    fetchSandboxImageStatus.mockResolvedValue(running());
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    await pollAfter(0);
    fetchSandboxImageStatus.mockResolvedValue(status({ pull: { state: "cancelled" } }));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await pollAfter(0);
    expect(banner()?.textContent).toBe(CANCELLED);
    await pollAfter(2000);
    expect(banner()?.textContent).toBe(CANCELLED);
  });

  it.each([
    { name: "the Daemon accepts the update", confirm: "reply" },
    { name: "a status shows it running", confirm: "status" },
  ])("offers Cancel only once $name, so it can't reach the Daemon before the pull", async ({ confirm }) => {
    let accept!: (r: { ok: boolean }) => void;
    pullSandboxImage.mockReturnValueOnce(new Promise((r) => (accept = r)));
    await mount(status());
    fireEvent.click(screen.getByRole("button", { name: "Update now" }));
    // The Daemon hasn't registered the pull yet.
    fetchSandboxImageStatus.mockResolvedValue(status());
    await pollAfter(0);
    expect(banner()?.textContent).toContain(DOWNLOADING);
    const early = screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement;
    expect(early.disabled).toBe(true);
    expect(early.title).toBe(CANCEL_NOT_READY);
    fireEvent.click(early);
    expect(cancelSandboxImagePull).not.toHaveBeenCalled();
    if (confirm === "reply") {
      // Hold the poll the reply sends, so only the acceptance confirms the pull.
      fetchSandboxImageStatus.mockReturnValueOnce(new Promise(() => {}));
      await act(async () => accept({ ok: true }));
    } else {
      fetchSandboxImageStatus.mockResolvedValue(running());
      await pollAfter(2000);
    }
    const ready = screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement;
    expect(ready.disabled).toBe(false);
    fireEvent.click(ready);
    expect(cancelSandboxImagePull).toHaveBeenCalledOnce();
  });

  it("retires a pending Cancel once a poll sees its pull end, so the same image restarting isn't blocked", async () => {
    let answer!: (r: { ok: boolean; pull?: SandboxImagePull }) => void;
    cancelSandboxImagePull.mockReturnValueOnce(new Promise((r) => (answer = r)));
    // Found by polling: no Update now here and no create notification.
    await mount(running());
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } }));
    await pollAfter(2000);
    // Another client pulls the same image again.
    fetchSandboxImageStatus.mockResolvedValue(running());
    await pollAfter(30_000);
    expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(false);
    // Hold the next poll, so what shows is the old reply's doing alone.
    fetchSandboxImageStatus.mockReturnValueOnce(new Promise(() => {}));
    await act(async () => answer({ ok: true, pull: { state: "cancelled" } }));
    expect(banner()?.textContent).toContain(DOWNLOADING);
  });

  it("offers no Update now or Retry on an outcome while the Daemon won't pull from the web", async () => {
    const cases: [SandboxImagePull, string, string][] = [
      [{ state: "cancelled" }, CANCELLED, "Update now"],
      [{ state: "failed", message: FAILED, error: "denied" } as SandboxImagePull, FAILED, "Retry"],
    ];
    for (const [pull, words, action] of cases) {
      for (const pull_blocked of [null, "read_only"] as const) {
        const { unmount } = await mount(running({}));
        fetchSandboxImageStatus.mockResolvedValue(status({ pull, pull_blocked }));
        await pollAfter(2000);
        const label = `${pull.state} ${pull_blocked}`;
        expect(banner()?.textContent, label).toBe(words);
        expect(screen.queryByRole("button", { name: action }) !== null, label).toBe(pull_blocked === null);
        unmount();
      }
    }
  });

  it("offers no Cancel while the Daemon won't pull from the web", async () => {
    await mount(status({ state: "current", pull: { state: "running" }, pull_blocked: "disabled" }));
    expect(banner()?.textContent).toBe(DOWNLOADING);
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
  });

  it("hides a running pull on ×, and shows its outcome when it ends", async () => {
    await mount(running());
    fireEvent.click(screen.getByRole("button", { name: "Hide sandbox image notice" }));
    expect(bar()).toBeNull();
    // Still running: the next poll keeps it hidden.
    await pollAfter(2000);
    expect(bar()).toBeNull();
    fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } }));
    await pollAfter(2000);
    expect(banner()?.textContent).toBe(UPDATED);
  });

  it.each([
    ["done", { state: "done" }, UPDATED],
    ["failed", { state: "failed", reason: "other", message: FAILED, error: "no space left" }, FAILED],
  ] as const)(
    "shows the outcome of a pull started elsewhere after an earlier outcome was dismissed (%s)",
    async (_, pull, words) => {
      await mount(status());
      fireEvent.click(screen.getByRole("button", { name: "Update now" }));
      fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull: { state: "done" } }));
      await pollAfter(0);
      fireEvent.click(screen.getByRole("button", { name: "Dismiss sandbox image notice" }));
      expect(bar()).toBeNull();
      // Another client starts the next pull; this page only sees it by polling.
      fetchSandboxImageStatus.mockResolvedValue(running());
      await pollAfter(30_000);
      expect(banner()?.textContent).toBe(DOWNLOADING);
      fetchSandboxImageStatus.mockResolvedValue(status({ state: "current", pull }));
      await pollAfter(2000);
      expect(banner()?.textContent).toBe(words);
    },
  );

  it("shows a pull's failure reason from the Daemon, with its runtime output under Details", async () => {
    await mount(running());
    fetchSandboxImageStatus.mockResolvedValue(
      status({
        state: "available",
        pull: {
          state: "failed",
          reason: "disk_space",
          message: "Couldn't download the sandbox image: the disk is full.",
          error: "no space left on device",
        },
      }),
    );
    await pollAfter(2000);
    expect(banner()?.textContent).toBe("Couldn't download the sandbox image: the disk is full.");
    fireEvent.click(screen.getByRole("button", { name: "Details" }));
    expect(screen.getByText("no space left on device").closest("[hidden]")).toBeNull();
  });

  it("polls every 30 s while an update can show and the tab is visible, skips while hidden, and every 2 s while running", async () => {
    let visibility: DocumentVisibilityState = "visible";
    const spy = vi.spyOn(document, "visibilityState", "get").mockImplementation(() => visibility);
    try {
      await mount(status({ state: "current" }));
      const calls = () => fetchSandboxImageStatus.mock.calls.length;
      expect(calls()).toBe(1);
      await pollAfter(29_999);
      expect(calls()).toBe(1);
      await pollAfter(1);
      expect(calls()).toBe(2);

      visibility = "hidden";
      await pollAfter(30_000 * 3);
      expect(calls()).toBe(2);
      // Back on screen, it asks at once, and a pull started elsewhere shows within the idle poll.
      visibility = "visible";
      fetchSandboxImageStatus.mockResolvedValue(running());
      await act(async () => document.dispatchEvent(new Event("visibilitychange")));
      await pollAfter(0);
      expect(calls()).toBe(3);
      expect(banner()?.textContent).toBe(DOWNLOADING);
      await pollAfter(2000);
      expect(calls()).toBe(4);
    } finally {
      spy.mockRestore();
    }
  });

  it("polls fast once a create that pulls is sent, so its download shows while it runs", async () => {
    await mount(status({ state: "current", checks_enabled: true }));
    const calls = () => fetchSandboxImageStatus.mock.calls.length;
    act(() => notifySandboxImagePullStarted());
    await pollAfter(0);
    expect(calls()).toBe(2);
    // The create may reach its pull only after its hooks: keep asking every 2 s for a while.
    await pollAfter(2000);
    expect(calls()).toBe(3);
    fetchSandboxImageStatus.mockResolvedValue(running());
    await pollAfter(2000);
    expect(banner()?.textContent).toBe(DOWNLOADING);
  });
});

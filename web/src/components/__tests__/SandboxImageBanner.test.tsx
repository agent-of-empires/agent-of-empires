// @vitest-environment jsdom
// Sandbox image banner (#4591): which state shows for a Daemon status, and the pull flow.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { SandboxImageStatus } from "../../lib/api";
import { useSandboxImageBanner } from "../../hooks/useSandboxImageBanner";
import { SandboxImageBanner } from "../SandboxImageBanner";

const fetchSandboxImageStatus = vi.fn();
const pullSandboxImage = vi.fn(() => Promise.resolve(true));
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

const banner = () => screen.queryByRole("status", { name: /sandbox image/i });
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
      ["config_unreadable", "Couldn't check the sandbox image"],
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
});

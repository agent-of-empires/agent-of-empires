// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { SandboxImageStatus } from "../../lib/api";

const fetchSandboxImageStatus = vi.fn();
vi.mock("../../lib/api", () => ({
  fetchSandboxImageStatus: () => fetchSandboxImageStatus(),
  pullSandboxImage: () => Promise.resolve({ ok: true }),
  dismissSandboxImage: () => Promise.resolve(true),
  cancelSandboxImagePull: () => Promise.resolve({ ok: true }),
}));
vi.mock("../../hooks/usePushSubscription", () => ({
  usePushSubscription: () => ({ state: { kind: "off" }, health: "healthy", enable: vi.fn() }),
}));
// Whether the connection banner, which outranks the sandbox one, is up.
let down = false;
vi.mock("../../hooks/useConnectionBanner", () => ({ useConnectionBanner: () => (down ? "down" : null) }));
vi.mock("../../hooks/useDashboardUpdate", () => ({ useDashboardUpdate: () => null }));
vi.mock("../../hooks/useUpdateBanner", () => ({ useUpdateBanner: () => null }));

import { BannerStack } from "../BannerStack";
import { SandboxImageProvider } from "../SandboxImageProvider";
import { SandboxPullChip } from "../SandboxPullChip";

function running(pull: Record<string, unknown> = {}): SandboxImageStatus {
  return {
    image: "sb:1",
    state: "current",
    dismissed: false,
    checks_enabled: true,
    sandbox_in_use: true,
    pull: { state: "running", phase: "downloading", ...pull },
    pull_blocked: null,
    pull_command: "docker pull sb:1",
    runtime: "Docker",
  } as SandboxImageStatus;
}

function App() {
  return (
    <SandboxImageProvider>
      <SandboxPullChip />
      <BannerStack />
    </SandboxImageProvider>
  );
}

const chip = () => screen.queryByTestId("sandbox-pull-chip");
const banner = () => screen.queryByTestId("sandbox-image-banner");

async function mount(status: SandboxImageStatus) {
  fetchSandboxImageStatus.mockResolvedValue(status);
  const utils = render(<App />);
  await act(() => vi.advanceTimersByTimeAsync(0));
  return utils;
}

beforeEach(() => {
  vi.useFakeTimers();
  fetchSandboxImageStatus.mockReset();
  down = false;
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("SandboxPullChip", () => {
  it("stands in for a hidden pull banner and brings it back on click", async () => {
    await mount(running({ layers_done: 3, layers_known: 7 }));
    expect(chip()).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Hide sandbox image notice" }));
    expect(banner()).toBeNull();
    const shown = screen.getByRole("button", { name: "Downloading sandbox image, 3 of 7 layers. Show details" });
    expect(shown.textContent).toBe("Image 3/7");
    fireEvent.click(shown);
    expect(banner()).not.toBeNull();
    expect(chip()).toBeNull();
  });

  it("drops Show details while a higher banner covers the pull, since a click can't show it", async () => {
    // (connection down, hidden with ×, layer counts, chip's accessible name or null)
    const cases: [boolean, boolean, Record<string, unknown>, string | null][] = [
      [true, false, { layers_done: 3, layers_known: 7 }, "Downloading sandbox image, 3 of 7 layers"],
      [true, true, {}, "Downloading sandbox image"],
      [false, true, {}, "Downloading sandbox image. Show details"],
      [false, false, {}, null],
    ];
    for (const [connectionDown, hide, pull, name] of cases) {
      const label = JSON.stringify({ connectionDown, hide, pull });
      const { rerender, unmount } = await mount(running(pull));
      if (hide) fireEvent.click(screen.getByRole("button", { name: "Hide sandbox image notice" }));
      down = connectionDown;
      rerender(<App />);
      await act(() => vi.advanceTimersByTimeAsync(0));
      expect(chip()?.getAttribute("aria-label") ?? null, label).toBe(name);
      if (name) {
        expect(chip()?.getAttribute("aria-disabled"), label).toBe(String(connectionDown));
        fireEvent.click(chip()!);
        // A click shows the banner only when nothing outranks it.
        expect(banner() !== null, label).toBe(!connectionDown);
      }
      unmount();
      down = false;
    }
  });

  it("goes away with the pull", async () => {
    await mount(running());
    fireEvent.click(screen.getByRole("button", { name: "Hide sandbox image notice" }));
    expect(chip()).not.toBeNull();
    fetchSandboxImageStatus.mockResolvedValue({ ...running(), pull: { state: "done" } });
    await act(() => vi.advanceTimersByTimeAsync(2000));
    expect(chip()).toBeNull();
    expect(screen.getByRole("status").textContent).toBe("Sandbox image updated. Applies to new Sessions.");
  });
});

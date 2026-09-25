// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { UpdateBanner } from "../UpdateBanner";
import type { UpdateStatus } from "../../lib/api";

const fetchUpdateStatus = vi.fn();
const dismissUpdate = vi.fn();

vi.mock("../../lib/api", () => ({
  fetchUpdateStatus: (...args: unknown[]) => fetchUpdateStatus(...args),
  dismissUpdate: (...args: unknown[]) => dismissUpdate(...args),
}));

function makeStatus(overrides?: Partial<UpdateStatus>): UpdateStatus {
  return {
    update_check_mode: "notify",
    current_version: "1.0.0",
    latest_version: "1.1.0",
    update_available: true,
    release_url: "https://example.com/releases/1.1.0",
    error: null,
    dismissed_version: null,
    ...overrides,
  };
}

beforeEach(() => {
  fetchUpdateStatus.mockReset();
  dismissUpdate.mockReset();
  dismissUpdate.mockResolvedValue(true);
});

afterEach(() => {
  cleanup();
});

describe("UpdateBanner", () => {
  it("renders the banner with both versions and the release link only when a URL is set", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus());
    render(<UpdateBanner />);

    const banner = await screen.findByRole("status");
    expect(banner.getAttribute("aria-label")).toBe("Update available: v1.1.0");
    expect(banner.textContent).toContain("v1.0.0");
    expect(banner.textContent).toContain("v1.1.0");
    const link = screen.getByText("Release notes") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://example.com/releases/1.1.0");
    cleanup();

    fetchUpdateStatus.mockResolvedValue(makeStatus({ release_url: null }));
    render(<UpdateBanner />);
    await screen.findByRole("status");
    expect(screen.queryByText("Release notes")).toBeNull();
  });

  it("renders nothing before the first poll or when there is nothing to show", async () => {
    fetchUpdateStatus.mockReturnValue(new Promise(() => {}));
    expect(render(<UpdateBanner />).container.firstChild).toBeNull();
    cleanup();

    // "off" mode has no separate client path: the server reports update_available: false, so only "auto" is
    // special-cased here.
    const cases: Partial<UpdateStatus>[] = [
      { update_available: false },
      { update_check_mode: "auto" },
      { dismissed_version: "1.1.0" },
    ];
    for (const overrides of cases) {
      fetchUpdateStatus.mockReset();
      fetchUpdateStatus.mockResolvedValue(makeStatus(overrides));
      const { container } = render(<UpdateBanner />);
      await waitFor(() => expect(fetchUpdateStatus).toHaveBeenCalled());
      expect(container.querySelector('[role="status"]'), JSON.stringify(overrides)).toBeNull();
      cleanup();
    }
  });

  it("dismiss hides the banner optimistically and persists via dismissUpdate", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus());
    const { container } = render(<UpdateBanner />);

    await screen.findByRole("status");
    fireEvent.click(screen.getByLabelText("Dismiss update notice"));

    expect(dismissUpdate).toHaveBeenCalledTimes(1);
    expect(dismissUpdate).toHaveBeenCalledWith("1.1.0");
    expect(container.querySelector('[role="status"]')).toBeNull();
  });
});

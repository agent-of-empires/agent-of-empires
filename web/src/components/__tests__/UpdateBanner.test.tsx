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
  it("renders nothing before the first poll resolves", () => {
    fetchUpdateStatus.mockReturnValue(new Promise(() => {}));
    const { container } = render(<UpdateBanner />);
    expect(container.firstChild).toBeNull();
  });

  it("renders the banner with both versions when an update is available", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus());
    render(<UpdateBanner />);

    const banner = await screen.findByRole("status");
    expect(banner.getAttribute("aria-label")).toBe("Update available: v1.1.0");
    expect(banner.textContent).toContain("v1.0.0");
    expect(banner.textContent).toContain("v1.1.0");

    const link = screen.getByText("Release notes") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://example.com/releases/1.1.0");
  });

  // "off" mode has no separate client path: the server reports update_available: false, so only "auto" is
  // special-cased here.
  it.each([
    ["no update is available", { update_available: false }],
    ["auto mode handles the install", { update_check_mode: "auto" }],
    ["off mode reports no update", { update_check_mode: "off", update_available: false }],
    ["the version was already dismissed server-side", { dismissed_version: "1.1.0" }],
  ] as [string, Partial<UpdateStatus>][])("renders nothing when %s", async (_name, overrides) => {
    fetchUpdateStatus.mockResolvedValue(makeStatus(overrides));
    const { container } = render(<UpdateBanner />);
    await waitFor(() => expect(fetchUpdateStatus).toHaveBeenCalled());
    expect(container.querySelector('[role="status"]')).toBeNull();
  });

  it("omits the release-notes link when release_url is null", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus({ release_url: null }));
    render(<UpdateBanner />);
    await screen.findByRole("status");
    expect(screen.queryByText("Release notes")).toBeNull();
  });

  it("dismiss hides the banner optimistically and persists via dismissUpdate", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus());
    const { container } = render(<UpdateBanner />);

    await screen.findByRole("status");
    const dismissBtn = screen.getByLabelText("Dismiss update notice");
    fireEvent.click(dismissBtn);

    expect(dismissUpdate).toHaveBeenCalledTimes(1);
    expect(dismissUpdate).toHaveBeenCalledWith("1.1.0");
    expect(container.querySelector('[role="status"]')).toBeNull();
  });
});

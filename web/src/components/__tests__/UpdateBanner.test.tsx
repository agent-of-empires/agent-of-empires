// @vitest-environment jsdom
//
// Tests for UpdateBanner. The banner polls `/api/system/update-status` on
// mount and renders a top-of-app notice when an update is available and the
// check mode is not `auto`/`off`. Dismiss persists server-side via
// dismissUpdate (keyed by latest_version) and hides the banner optimistically.

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
    web_poll_interval_minutes: 60,
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

  it("renders nothing when no update is available", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus({ update_available: false }));
    const { container } = render(<UpdateBanner />);
    await waitFor(() => expect(fetchUpdateStatus).toHaveBeenCalled());
    expect(container.querySelector('[role="status"]')).toBeNull();
  });

  it("renders nothing in auto mode even when an update is available", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus({ update_check_mode: "auto" }));
    const { container } = render(<UpdateBanner />);
    await waitFor(() => expect(fetchUpdateStatus).toHaveBeenCalled());
    expect(container.querySelector('[role="status"]')).toBeNull();
  });

  // In "off" mode the server reports update_available: false, so the banner
  // never has anything to render; the client only special-cases "auto". The
  // no-update suppression is the off-mode contract, covered above.
  it("renders nothing in off mode (server reports no update available)", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus({ update_check_mode: "off", update_available: false }));
    const { container } = render(<UpdateBanner />);
    await waitFor(() => expect(fetchUpdateStatus).toHaveBeenCalled());
    expect(container.querySelector('[role="status"]')).toBeNull();
  });

  it("renders nothing when the version was already dismissed server-side", async () => {
    fetchUpdateStatus.mockResolvedValue(makeStatus({ dismissed_version: "1.1.0" }));
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

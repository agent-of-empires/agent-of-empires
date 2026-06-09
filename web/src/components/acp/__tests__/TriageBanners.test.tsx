// @vitest-environment jsdom
//
// Vitest coverage for the acp-view banner variants introduced in
// #1581. Each banner is a tiny presentational component, but they
// own the copy the user sees the moment they archive or snooze a
// session, and any drift between the testid + the banner switch in
// StructuredView's variant picker would silently swap which message
// renders.

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { ArchivedWorkerStoppedBanner, SnoozedWorkerStoppedBanner } from "../StructuredView";

afterEach(() => {
  cleanup();
});

describe("ArchivedWorkerStoppedBanner", () => {
  it("renders the archived copy with the sessionId-scoped testid", () => {
    render(<ArchivedWorkerStoppedBanner sessionId="abc-123" />);
    const banner = screen.getByTestId("acp-archived-banner-abc-123");
    expect(banner).not.toBeNull();
    expect(banner.textContent).toContain("Session archived");
    // Mentions the sidebar path so users know how to unblock.
    expect(banner.textContent).toContain("Unarchive");
  });

  it("isolates banners by sessionId", () => {
    render(<ArchivedWorkerStoppedBanner sessionId="alpha" />);
    expect(screen.queryByTestId("acp-archived-banner-alpha")).not.toBeNull();
    expect(screen.queryByTestId("acp-archived-banner-beta")).toBeNull();
  });
});

describe("SnoozedWorkerStoppedBanner", () => {
  it("renders the snoozed copy with a localized wake-time", () => {
    render(<SnoozedWorkerStoppedBanner sessionId="abc-123" snoozedUntil="2099-01-01T00:00:00Z" />);
    const banner = screen.getByTestId("acp-snoozed-banner-abc-123");
    expect(banner).not.toBeNull();
    expect(banner.textContent).toContain("Session snoozed");
    // The wake time renders via `Date#toLocaleString`. We can't
    // assert the exact string (depends on host TZ + locale), but
    // we can confirm a year fragment appears so the wall-clock
    // formatting path ran instead of falling through to the raw
    // ISO.
    expect(banner.textContent).toMatch(/2099|2098/);
  });

  it("mentions the Unsnooze affordance", () => {
    render(<SnoozedWorkerStoppedBanner sessionId="abc-123" snoozedUntil="2099-01-01T00:00:00Z" />);
    const banner = screen.getByTestId("acp-snoozed-banner-abc-123");
    expect(banner.textContent).toContain("Unsnooze");
  });

  it("falls through to the raw ISO when the timestamp is unparseable", () => {
    // Defensive: the server gates snoozed_until on `is_snoozed()`,
    // but an unparseable string would otherwise crash
    // `Date.toLocaleString`. We render the raw value instead.
    render(<SnoozedWorkerStoppedBanner sessionId="abc-123" snoozedUntil="not-a-date" />);
    const banner = screen.getByTestId("acp-snoozed-banner-abc-123");
    expect(banner.textContent).toContain("not-a-date");
  });
});

// @vitest-environment jsdom

import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { PushState } from "../../hooks/usePushSubscription";
import type { PushHealth } from "../../lib/pushHealth";

const enable = vi.fn();
let hook: { state: PushState; health: PushHealth };

vi.mock("../../hooks/usePushSubscription", () => ({
  usePushSubscription: () => ({ ...hook, enable }),
}));

import { PushHealthBanner } from "../PushHealthBanner";

beforeEach(() => {
  localStorage.clear();
  enable.mockClear();
  hook = { state: { kind: "off" }, health: "revoked" };
});

const reEnable = () => screen.queryByRole("button", { name: "Re-enable" });

describe("PushHealthBanner", () => {
  it.each<[PushHealth, boolean]>([
    ["revoked", true],
    ["key-mismatch", true],
    ["delivery-failed", true],
    ["permission-denied", true],
    ["healthy", false],
    ["server-forgot", false],
    ["not-wanted", false],
    ["unknown", false],
  ])("health %s shows the banner: %s", (health, shown) => {
    hook.health = health;
    render(<PushHealthBanner />);
    expect(screen.queryByRole("alert") !== null).toBe(shown);
  });

  it("runs enable() synchronously from the Re-enable click", () => {
    render(<PushHealthBanner />);
    fireEvent.click(reEnable()!);
    expect(enable).toHaveBeenCalledTimes(1);
  });

  it("offers settings guidance instead of a button when permission is denied", () => {
    hook.health = "permission-denied";
    render(<PushHealthBanner />);
    expect(screen.getByRole("alert").textContent).toContain("Notifications are blocked");
    expect(reEnable()).toBeNull();
  });

  it("stays dismissed for the same failure but returns for a different one", () => {
    const { rerender } = render(<PushHealthBanner />);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss notifications notice" }));
    expect(screen.queryByRole("alert")).toBeNull();

    rerender(<PushHealthBanner />);
    expect(screen.queryByRole("alert")).toBeNull();

    hook.health = "key-mismatch";
    rerender(<PushHealthBanner />);
    expect(screen.queryByRole("alert")).not.toBeNull();
  });
});

// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { PasskeySettings } from "../PasskeySettings";

const fetchPasskeys = vi.fn();
const registerPasskey = vi.fn();
const removePasskey = vi.fn();
const passkeysSupported = vi.fn();
vi.mock("../../lib/api", () => ({
  fetchPasskeys: () => fetchPasskeys(),
  registerPasskey: (...args: unknown[]) => registerPasskey(...args),
  removePasskey: (...args: unknown[]) => removePasskey(...args),
  passkeysSupported: () => passkeysSupported(),
}));

const phone = {
  credential_id: "c1",
  label: "Phone",
  origin: "https://aoe.example.com",
  created_at_ms: 1,
  last_used_at_ms: null,
};

beforeEach(() => {
  for (const fn of [fetchPasskeys, registerPasskey, removePasskey, passkeysSupported]) fn.mockReset();
  passkeysSupported.mockReturnValue(true);
});
afterEach(cleanup);

describe("PasskeySettings", () => {
  it("renders nothing without browser support", async () => {
    passkeysSupported.mockReturnValue(false);
    fetchPasskeys.mockResolvedValue([]);
    const { container } = render(<PasskeySettings />);
    await waitFor(() => expect(fetchPasskeys).toHaveBeenCalled());
    expect(container.textContent).toBe("");
  });

  it("lists passkeys and reloads after adding one", async () => {
    fetchPasskeys
      .mockResolvedValueOnce([phone])
      .mockResolvedValueOnce([phone, { ...phone, credential_id: "c2", label: "Laptop" }]);
    registerPasskey.mockResolvedValue({ ok: true });
    render(<PasskeySettings />);
    expect(await screen.findByText("Phone")).toBeTruthy();
    expect(screen.getByText("never used")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Add a passkey for this device" }));
    expect(await screen.findByText("Laptop")).toBeTruthy();
    expect(registerPasskey).toHaveBeenCalledOnce();
  });

  it("shows enrolment errors but not a dismissed prompt", async () => {
    fetchPasskeys.mockResolvedValue([]);
    render(<PasskeySettings />);
    const add = await screen.findByRole("button", { name: "Add a passkey for this device" });

    registerPasskey.mockResolvedValueOnce({ ok: false, cancelled: true });
    fireEvent.click(add);
    await waitFor(() => expect(registerPasskey).toHaveBeenCalledOnce());
    await waitFor(() => expect((add as HTMLButtonElement).disabled).toBe(false));
    expect(screen.queryByText("nope")).toBeNull();

    registerPasskey.mockResolvedValueOnce({ ok: false, error: "nope" });
    fireEvent.click(add);
    expect(await screen.findByText("nope")).toBeTruthy();
  });

  it("removes a passkey and reloads", async () => {
    fetchPasskeys.mockResolvedValueOnce([phone]).mockResolvedValueOnce([]);
    removePasskey.mockResolvedValue(true);
    render(<PasskeySettings />);
    fireEvent.click(await screen.findByRole("button", { name: "Remove" }));
    await waitFor(() => expect(screen.queryByText("Phone")).toBeNull());
    expect(removePasskey).toHaveBeenCalledWith("c1");
  });
});

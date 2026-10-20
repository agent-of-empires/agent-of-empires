// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

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
  it("renders nothing without browser support once the list has loaded", async () => {
    passkeysSupported.mockReturnValue(false);
    let resolveList!: (list: unknown[]) => void;
    fetchPasskeys.mockReturnValue(new Promise((resolve) => (resolveList = resolve)));
    const { container } = render(<PasskeySettings />);
    await waitFor(() => expect(fetchPasskeys).toHaveBeenCalled());
    await act(async () => resolveList([]));
    expect(container.textContent).toBe("");
  });

  it("keeps list and remove without browser support but hides adding", async () => {
    passkeysSupported.mockReturnValue(false);
    fetchPasskeys.mockResolvedValue([phone]);
    render(<PasskeySettings />);
    expect(await screen.findByText("Phone")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Remove" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Add a passkey for this device" })).toBeNull();
  });

  it("offers Retry when the list fails to load", async () => {
    fetchPasskeys.mockResolvedValueOnce(null).mockResolvedValueOnce([phone]);
    render(<PasskeySettings />);
    expect(await screen.findByText("Could not load passkeys")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByText("Phone")).toBeTruthy();
    expect(screen.queryByText("Could not load passkeys")).toBeNull();
  });

  it("enrols with the label the user typed", async () => {
    fetchPasskeys.mockResolvedValue([]);
    registerPasskey.mockResolvedValue({ ok: true });
    render(<PasskeySettings />);
    const input = (await screen.findByLabelText("Passkey label")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "Work laptop" } });
    fireEvent.click(screen.getByRole("button", { name: "Add a passkey for this device" }));
    await waitFor(() => expect(registerPasskey).toHaveBeenCalledWith("Work laptop"));
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

  it("shows an error when removal fails and keeps the passkey", async () => {
    fetchPasskeys.mockResolvedValue([phone]);
    removePasskey.mockResolvedValue(false);
    render(<PasskeySettings />);
    fireEvent.click(await screen.findByRole("button", { name: "Remove" }));
    expect(await screen.findByText("Could not remove passkey")).toBeTruthy();
    expect(screen.getByText("Phone")).toBeTruthy();
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

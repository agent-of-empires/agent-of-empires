// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { LoginPage } from "../LoginPage";

const login = vi.fn();
const loginStatus = vi.fn();
const loginWithPasskey = vi.fn();
const passkeysSupported = vi.fn();
vi.mock("../../lib/api", () => ({
  login: (...args: unknown[]) => login(...args),
  loginStatus: () => loginStatus(),
  loginWithPasskey: () => loginWithPasskey(),
  passkeysSupported: () => passkeysSupported(),
}));

beforeEach(() => {
  login.mockReset();
  loginStatus.mockReset();
  loginWithPasskey.mockReset();
  passkeysSupported.mockReset();
});
afterEach(cleanup);

function input() {
  return screen.getByPlaceholderText("Enter passphrase") as HTMLInputElement;
}

describe("LoginPage", () => {
  it("disables Sign in until a passphrase is typed", () => {
    render(<LoginPage onSuccess={() => {}} />);
    const btn = screen.getByRole("button", { name: "Sign in" }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    fireEvent.change(input(), { target: { value: "hunter2" } });
    expect(btn.disabled).toBe(false);
  });

  it("calls onSuccess when login resolves ok", async () => {
    login.mockResolvedValue({ ok: true });
    const onSuccess = vi.fn();
    render(<LoginPage onSuccess={onSuccess} />);
    fireEvent.change(input(), { target: { value: "hunter2" } });
    fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
    await waitFor(() => expect(onSuccess).toHaveBeenCalledOnce());
    expect(login).toHaveBeenCalledWith("hunter2");
  });

  it("shows the error on a failed login", async () => {
    login.mockResolvedValue({ ok: false, error: "nope" });
    render(<LoginPage onSuccess={() => {}} />);
    fireEvent.change(input(), { target: { value: "bad" } });
    fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByText("nope")).toBeTruthy();
  });

  it("toggles passphrase visibility", () => {
    render(<LoginPage onSuccess={() => {}} />);
    expect(input().type).toBe("password");
    fireEvent.click(screen.getByLabelText("Show passphrase"));
    expect(input().type).toBe("text");
    fireEvent.click(screen.getByLabelText("Hide passphrase"));
    expect(input().type).toBe("password");
  });

  describe("passkey sign-in", () => {
    const passkeyButton = () => screen.queryByRole("button", { name: "Sign in with passkey" });

    it("offers the button only when the browser supports it and a passkey is enrolled", async () => {
      for (const [supported, available, shown] of [
        [false, true, false],
        [true, false, false],
        [true, true, true],
      ] as const) {
        passkeysSupported.mockReturnValue(supported);
        loginStatus.mockResolvedValue({ required: true, passkey_available: available });
        const { unmount } = render(<LoginPage onSuccess={() => {}} />);
        await waitFor(() => expect(loginStatus).toHaveBeenCalledTimes(supported ? 1 : 0));
        await waitFor(() => expect(passkeyButton() !== null).toBe(shown));
        unmount();
        loginStatus.mockClear();
      }
    });

    async function renderWithPasskey(onSuccess = () => {}) {
      passkeysSupported.mockReturnValue(true);
      loginStatus.mockResolvedValue({ required: true, passkey_available: true });
      render(<LoginPage onSuccess={onSuccess} />);
      return await screen.findByRole("button", { name: "Sign in with passkey" });
    }

    it("calls onSuccess when the passkey login resolves ok", async () => {
      loginWithPasskey.mockResolvedValue({ ok: true });
      const onSuccess = vi.fn();
      fireEvent.click(await renderWithPasskey(onSuccess));
      await waitFor(() => expect(onSuccess).toHaveBeenCalledOnce());
    });

    it("stays silent on a dismissed prompt and shows server errors", async () => {
      const button = (await renderWithPasskey()) as HTMLButtonElement;

      loginWithPasskey.mockResolvedValueOnce({ ok: false, cancelled: true });
      fireEvent.click(button);
      await waitFor(() => expect(loginWithPasskey).toHaveBeenCalledOnce());
      await waitFor(() => expect(button.disabled).toBe(false));
      expect(screen.queryByText("Passkey verification failed")).toBeNull();

      loginWithPasskey.mockResolvedValueOnce({ ok: false, error: "Passkey verification failed" });
      fireEvent.click(button);
      expect(await screen.findByText("Passkey verification failed")).toBeTruthy();
    });
  });
});

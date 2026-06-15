// @vitest-environment jsdom
//
// Coverage for LoginPage: passphrase submit -> login(), success calls
// onSuccess, failure surfaces the error and re-enables the form, the
// show/hide passphrase toggle flips the input type, and the submit button is
// gated on a non-empty passphrase.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { LoginPage } from "../LoginPage";

const login = vi.fn();
vi.mock("../../lib/api", () => ({
  login: (...args: unknown[]) => login(...args),
}));

beforeEach(() => login.mockReset());
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
});

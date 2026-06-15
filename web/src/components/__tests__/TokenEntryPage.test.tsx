// @vitest-environment jsdom
//
// Coverage for TokenEntryPage: extractToken handles both a raw token and a
// full dashboard URL; a verified token saves and calls onSuccess; a rejected
// token shows the error and re-enables the form.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { TokenEntryPage } from "../TokenEntryPage";

const verifyToken = vi.fn();
const saveToken = vi.fn();
const resetTokenExpired = vi.fn();

vi.mock("../../lib/api", () => ({ verifyToken: () => verifyToken() }));
vi.mock("../../lib/token", () => ({ saveToken: (t: string) => saveToken(t) }));
vi.mock("../../lib/fetchInterceptor", () => ({ resetTokenExpired: () => resetTokenExpired() }));

beforeEach(() => {
  verifyToken.mockReset();
  saveToken.mockReset();
  resetTokenExpired.mockReset();
});
afterEach(cleanup);

function input() {
  return screen.getByPlaceholderText("Paste token or URL") as HTMLInputElement;
}

describe("TokenEntryPage", () => {
  it("saves a raw token and calls onSuccess when verified", async () => {
    verifyToken.mockResolvedValue(true);
    const onSuccess = vi.fn();
    render(<TokenEntryPage onSuccess={onSuccess} />);
    fireEvent.change(input(), { target: { value: "deadbeef" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    await waitFor(() => expect(onSuccess).toHaveBeenCalledOnce());
    expect(saveToken).toHaveBeenCalledWith("deadbeef");
  });

  it("extracts the token query param from a full URL", async () => {
    verifyToken.mockResolvedValue(true);
    render(<TokenEntryPage onSuccess={() => {}} />);
    fireEvent.change(input(), { target: { value: "https://host:8080/?token=fromurl" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    await waitFor(() => expect(saveToken).toHaveBeenCalledWith("fromurl"));
  });

  it("shows an error and stays interactive when the token is rejected", async () => {
    verifyToken.mockResolvedValue(false);
    render(<TokenEntryPage onSuccess={() => {}} />);
    fireEvent.change(input(), { target: { value: "bad" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    expect(await screen.findByText(/Invalid token/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Connect" }) as HTMLButtonElement).disabled).toBe(false);
  });
});

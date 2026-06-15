// @vitest-environment jsdom
//
// Tests for ElevationPrompt. The prompt is hidden until the global
// ELEVATION_REQUIRED_EVENT fires (dispatched by the fetch interceptor on a
// 403 elevation_required). Submitting the passphrase calls elevateLogin; on
// success the modal closes, on failure it shows the error and stays open.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ElevationPrompt } from "../ElevationPrompt";
import { ELEVATION_REQUIRED_EVENT } from "../../lib/fetchInterceptor";

const elevateLogin = vi.fn();

vi.mock("../../lib/api", () => ({
  elevateLogin: (...args: unknown[]) => elevateLogin(...args),
}));

// The component listens for a window-level CustomEvent and updates state in
// the handler; dispatch inside act so React flushes the open state before we
// assert against the rendered dialog.
function fireElevationRequired() {
  act(() => {
    window.dispatchEvent(new CustomEvent(ELEVATION_REQUIRED_EVENT));
  });
}

function getPassphraseInput() {
  return screen.getByPlaceholderText("Enter passphrase") as HTMLInputElement;
}

beforeEach(() => {
  elevateLogin.mockReset();
});

afterEach(() => {
  cleanup();
});

describe("ElevationPrompt", () => {
  it("renders nothing until the elevation-required event fires", () => {
    const { container } = render(<ElevationPrompt />);
    expect(container.firstChild).toBeNull();
  });

  it("opens the dialog when the elevation-required event fires", () => {
    render(<ElevationPrompt />);
    fireElevationRequired();
    const dialog = screen.getByRole("dialog");
    expect(dialog.getAttribute("aria-modal")).toBe("true");
    expect(screen.getByText("Confirm passphrase")).toBeTruthy();
  });

  it("Confirm is disabled until a non-empty passphrase is entered", () => {
    render(<ElevationPrompt />);
    fireElevationRequired();
    const confirm = screen.getByText("Confirm") as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);

    fireEvent.change(getPassphraseInput(), { target: { value: "hunter2" } });
    expect(confirm.disabled).toBe(false);
  });

  it("submitting calls elevateLogin with the passphrase and closes on success", async () => {
    elevateLogin.mockResolvedValue({ ok: true, elevated_until_secs: 900 });
    const { container } = render(<ElevationPrompt />);
    fireElevationRequired();

    fireEvent.change(getPassphraseInput(), { target: { value: "hunter2" } });
    fireEvent.click(screen.getByText("Confirm"));

    expect(elevateLogin).toHaveBeenCalledTimes(1);
    expect(elevateLogin).toHaveBeenCalledWith("hunter2");
    await waitFor(() => expect(container.querySelector('[role="dialog"]')).toBeNull());
  });

  it("shows the error and stays open when elevateLogin is denied", async () => {
    elevateLogin.mockResolvedValue({ ok: false, error: "Incorrect passphrase" });
    render(<ElevationPrompt />);
    fireElevationRequired();

    fireEvent.change(getPassphraseInput(), { target: { value: "wrong" } });
    fireEvent.click(screen.getByText("Confirm"));

    const err = await screen.findByText("Incorrect passphrase");
    expect(err).toBeTruthy();
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("falls back to a default error message when none is provided", async () => {
    elevateLogin.mockResolvedValue({ ok: false });
    render(<ElevationPrompt />);
    fireElevationRequired();

    fireEvent.change(getPassphraseInput(), { target: { value: "wrong" } });
    fireEvent.click(screen.getByText("Confirm"));

    expect(await screen.findByText("Could not confirm passphrase")).toBeTruthy();
  });

  it("disables the input and shows Confirming... while the request is in flight", async () => {
    let resolveElevate: ((v: { ok: boolean }) => void) | null = null;
    elevateLogin.mockReturnValue(
      new Promise((resolve) => {
        resolveElevate = resolve;
      }),
    );
    render(<ElevationPrompt />);
    fireElevationRequired();

    fireEvent.change(getPassphraseInput(), { target: { value: "hunter2" } });
    fireEvent.click(screen.getByText("Confirm"));

    await waitFor(() => expect(screen.getByText("Confirming...")).toBeTruthy());
    expect(getPassphraseInput().disabled).toBe(true);
    // A second submit while in flight must not fire elevateLogin again.
    fireEvent.submit(getPassphraseInput().closest("form")!);
    expect(elevateLogin).toHaveBeenCalledTimes(1);

    resolveElevate?.({ ok: true });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("Cancel closes the dialog without calling elevateLogin", () => {
    render(<ElevationPrompt />);
    fireElevationRequired();

    fireEvent.click(screen.getByText("Cancel"));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(elevateLogin).not.toHaveBeenCalled();
  });

  it("clicking the backdrop closes the dialog", () => {
    render(<ElevationPrompt />);
    fireElevationRequired();

    const dialog = screen.getByRole("dialog");
    fireEvent.click(dialog);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("submitting a whitespace-only passphrase does not call elevateLogin", () => {
    render(<ElevationPrompt />);
    fireElevationRequired();

    const input = getPassphraseInput();
    fireEvent.change(input, { target: { value: "   " } });
    fireEvent.submit(input.closest("form")!);
    expect(elevateLogin).not.toHaveBeenCalled();
  });
});

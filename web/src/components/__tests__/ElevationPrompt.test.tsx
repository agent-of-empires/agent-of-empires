// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ElevationPrompt } from "../ElevationPrompt";
import { ELEVATION_REQUIRED_EVENT } from "../../lib/fetchInterceptor";

const elevateLogin = vi.fn();

vi.mock("../../lib/api", () => ({
  elevateLogin: (...args: unknown[]) => elevateLogin(...args),
}));

// The component listens for a window-level CustomEvent and updates state in the handler; dispatch inside act so
// React flushes the open state before we assert against the rendered dialog.
function fireElevationRequired() {
  act(() => {
    window.dispatchEvent(new CustomEvent(ELEVATION_REQUIRED_EVENT));
  });
}

function getPassphraseInput() {
  return screen.getByPlaceholderText("Enter passphrase") as HTMLInputElement;
}

function openPrompt() {
  const utils = render(<ElevationPrompt />);
  fireElevationRequired();
  return utils;
}

function confirmWith(passphrase: string) {
  fireEvent.change(getPassphraseInput(), { target: { value: passphrase } });
  fireEvent.click(screen.getByText("Confirm"));
}

beforeEach(() => {
  elevateLogin.mockReset();
});

describe("ElevationPrompt", () => {
  it("opens only when the elevation-required event fires", () => {
    const { container } = render(<ElevationPrompt />);
    expect(container.firstChild).toBeNull();
    fireElevationRequired();
    expect(screen.getByRole("dialog").getAttribute("aria-modal")).toBe("true");
  });

  it("does not submit an empty or whitespace-only passphrase", () => {
    openPrompt();
    const confirm = screen.getByText("Confirm") as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.change(getPassphraseInput(), { target: { value: "hunter2" } });
    expect(confirm.disabled).toBe(false);

    fireEvent.change(getPassphraseInput(), { target: { value: "   " } });
    fireEvent.submit(getPassphraseInput().closest("form")!);
    expect(elevateLogin).not.toHaveBeenCalled();
  });

  it("submitting calls elevateLogin with the passphrase and closes on success", async () => {
    elevateLogin.mockResolvedValue({ ok: true, elevated_until_secs: 900 });
    const { container } = openPrompt();

    confirmWith("hunter2");

    expect(elevateLogin).toHaveBeenCalledTimes(1);
    expect(elevateLogin).toHaveBeenCalledWith("hunter2");
    await waitFor(() => expect(container.querySelector('[role="dialog"]')).toBeNull());
  });

  it.each([
    [{ ok: false, error: "Incorrect passphrase" }, "Incorrect passphrase"],
    [{ ok: false }, "Could not confirm passphrase"],
  ])("a denial keeps the dialog open and shows its message", async (result, message) => {
    elevateLogin.mockResolvedValue(result);
    openPrompt();

    confirmWith("wrong");

    expect(await screen.findByText(message)).toBeTruthy();
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("disables the input and shows Confirming... while the request is in flight", async () => {
    let resolveElevate: ((v: { ok: boolean }) => void) | null = null;
    elevateLogin.mockReturnValue(
      new Promise((resolve) => {
        resolveElevate = resolve;
      }),
    );
    openPrompt();

    confirmWith("hunter2");

    await waitFor(() => expect(screen.getByText("Confirming...")).toBeTruthy());
    expect(getPassphraseInput().disabled).toBe(true);
    // A second submit while in flight must not fire elevateLogin again.
    fireEvent.submit(getPassphraseInput().closest("form")!);
    expect(elevateLogin).toHaveBeenCalledTimes(1);

    resolveElevate?.({ ok: true });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("Cancel and backdrop clicks close the dialog without calling elevateLogin", () => {
    openPrompt();
    fireEvent.click(screen.getByText("Cancel"));
    expect(screen.queryByRole("dialog")).toBeNull();

    fireElevationRequired();
    fireEvent.click(screen.getByRole("dialog"));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(elevateLogin).not.toHaveBeenCalled();
  });
});

// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SessionErrorPanel } from "../SessionErrorPanel";
import { makeSession } from "./fixtures";
import type { SessionResponse } from "../../lib/types";

const { restartSession, writeClipboard } = vi.hoisted(() => ({
  restartSession: vi.fn<(id: string) => Promise<{ ok: boolean; message?: string }>>(),
  writeClipboard: vi.fn(async (_text: string) => true),
}));
vi.mock("../../lib/api", () => ({ restartSession }));
vi.mock("../../lib/clipboard", () => ({ writeClipboard }));

afterEach(() => {
  cleanup();
  restartSession.mockReset();
  writeClipboard.mockClear();
});

const ERROR = "resume failed for sid 55fd0b02; preserved for explicit retry\n  at line two";
const session = (over: Partial<SessionResponse> = {}) =>
  makeSession({ id: "e1", title: "Groceries", status: "Error", last_error: ERROR, ...over });

describe("SessionErrorPanel", () => {
  it.each<[string, Partial<SessionResponse>, boolean, string, string | null, boolean]>([
    ["a recorded error", {}, false, "This is the most recent error AoE recorded for this Session.", ERROR, true],
    ["no recorded error", { last_error: null }, false, "No details were recorded. Restart to try again.", null, true],
    [
      "a whitespace-only error is none",
      { last_error: " \n " },
      false,
      "No details were recorded. Restart to try again.",
      null,
      true,
    ],
    ["read-only, no recorded error", { last_error: null }, true, "No details were recorded.", null, false],
    [
      "read-only, a recorded error",
      {},
      true,
      "This is the most recent error AoE recorded for this Session.",
      ERROR,
      false,
    ],
    ["being deleted", { status: "Deleting" }, false, "This Session is being deleted.", ERROR, false],
  ])("%s", (_name, over, readOnly, lead, shown, restartable) => {
    render(<SessionErrorPanel session={session(over)} readOnly={readOnly} />);
    expect(screen.getByRole("heading").textContent).toBe("Groceries reported an error");
    expect(screen.getByText(lead)).toBeTruthy();
    // Verbatim, every line.
    expect(screen.queryByTestId("session-error-text")?.textContent ?? null).toBe(shown);
    expect(screen.queryByTestId("session-error-copy") !== null).toBe(shown !== null);
    expect(screen.queryByTestId("session-error-restart") !== null).toBe(restartable);
  });

  it("copies the full error", async () => {
    render(<SessionErrorPanel session={session()} readOnly={false} />);
    fireEvent.click(screen.getByTestId("session-error-copy"));
    await waitFor(() => expect(screen.getByTestId("session-error-copy").textContent).toBe("Copied"));
    expect(writeClipboard).toHaveBeenCalledWith(ERROR);
  });

  it("restarts once, disabled while it's pending, and shows a refusal", async () => {
    let answer!: (result: { ok: boolean; message?: string }) => void;
    restartSession.mockReturnValue(new Promise((resolve) => (answer = resolve)));
    render(<SessionErrorPanel session={session()} readOnly={false} />);
    const restart = screen.getByTestId<HTMLButtonElement>("session-error-restart");
    fireEvent.click(restart);
    expect(restart.disabled).toBe(true);
    expect(restart.textContent).toBe("Restarting…");
    fireEvent.click(restart);
    expect(restartSession).toHaveBeenCalledTimes(1);
    expect(restartSession).toHaveBeenCalledWith("e1");

    await act(async () => answer({ ok: false, message: "session is busy" }));
    expect(screen.getByTestId("session-error-restart-failed").textContent).toBe("Restart failed: session is busy");
    expect(restart.disabled).toBe(false);
  });

  it("names an untitled Session generically", () => {
    render(<SessionErrorPanel session={session({ title: "  " })} readOnly={false} />);
    expect(screen.getByRole("heading").textContent).toBe("This Session reported an error");
  });
});

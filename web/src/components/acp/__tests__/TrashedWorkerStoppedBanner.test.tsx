// @vitest-environment jsdom
//
// Coverage for TrashedWorkerStoppedBanner (#2489, #2593): the banner shown in
// the structured view when a trashed session's worker is stopped. The variant
// selection is covered by workerStoppedBanner.ts tests; this pins the banner's
// own render (copy + testid keyed by session id) and the in-place Restore
// button (#2593).

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { TrashedWorkerStoppedBanner } from "../StructuredView";

afterEach(cleanup);

describe("TrashedWorkerStoppedBanner (#2489, #2593)", () => {
  it("renders the trash notice keyed by session id", () => {
    render(<TrashedWorkerStoppedBanner sessionId="sess-9" />);
    expect(screen.getByTestId("acp-trashed-banner-sess-9")).toBeTruthy();
    expect(screen.getByText("Session in trash")).toBeTruthy();
    expect(screen.getByText(/read-only/)).toBeTruthy();
  });

  it("shows no Restore button when onRestore is omitted (read-only)", () => {
    render(<TrashedWorkerStoppedBanner sessionId="sess-9" />);
    expect(screen.queryByRole("button", { name: "Restore" })).toBeNull();
  });

  it("restores in place and shows a pending label while the call is in flight", () => {
    let resolve!: (ok: boolean) => void;
    const onRestore = vi.fn(() => new Promise<boolean>((r) => (resolve = r)));

    render(<TrashedWorkerStoppedBanner sessionId="sess-9" onRestore={onRestore} />);
    fireEvent.click(screen.getByRole("button", { name: "Restore" }));

    expect(onRestore).toHaveBeenCalledTimes(1);
    // Button flips to the pending label and disables so a double-click can't
    // fire a second restore.
    const pending = screen.getByRole("button", { name: "Restoring…" });
    expect((pending as HTMLButtonElement).disabled).toBe(true);

    // A second click while pending is a no-op.
    fireEvent.click(pending);
    expect(onRestore).toHaveBeenCalledTimes(1);

    resolve(true);
  });

  it("resets the pending state when restore resolves false so the user can retry", async () => {
    const onRestore = vi.fn(() => Promise.resolve(false));

    render(<TrashedWorkerStoppedBanner sessionId="sess-9" onRestore={onRestore} />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Restore" }));
    });

    // The banner stays and the button returns to the actionable label.
    await waitFor(() => expect(screen.getByRole("button", { name: "Restore" })).toBeTruthy());
    expect((screen.getByRole("button", { name: "Restore" }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("resets the pending state when restore rejects", async () => {
    const onRestore = vi.fn(() => Promise.reject(new Error("boom")));

    render(<TrashedWorkerStoppedBanner sessionId="sess-9" onRestore={onRestore} />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Restore" }));
    });

    await waitFor(() => expect(screen.getByRole("button", { name: "Restore" })).toBeTruthy());
    expect((screen.getByRole("button", { name: "Restore" }) as HTMLButtonElement).disabled).toBe(false);
  });
});

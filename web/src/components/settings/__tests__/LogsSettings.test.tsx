// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";

import type { LogsSnapshot } from "../../../lib/api";

const fetchLogs = vi.fn<[number, AbortSignal?], Promise<LogsSnapshot>>();

vi.mock("../../../lib/api", () => ({
  fetchLogs: (lines: number, signal?: AbortSignal) => fetchLogs(lines, signal),
}));

// Imported after the mock is registered.
import { LogsSettings } from "../LogsSettings";

const CONTENT = "INFO started\nWARN token=[REDACTED]\n";

beforeEach(() => {
  fetchLogs.mockReset();
});
afterEach(() => vi.unstubAllGlobals());

describe("LogsSettings", () => {
  it("shows the fetched tail and downloads exactly those bytes", async () => {
    fetchLogs.mockResolvedValue({ status: "available", content: CONTENT, lineCount: 2, truncated: true });
    const blobs: Blob[] = [];
    vi.stubGlobal("URL", {
      ...URL,
      createObjectURL: vi.fn((blob: Blob) => {
        blobs.push(blob);
        return "blob:stub";
      }),
      revokeObjectURL: vi.fn(),
    });

    const { findByTestId, getByTestId, getByText } = render(<LogsSettings />);
    expect((await findByTestId("logs-content")).textContent).toBe(CONTENT);
    expect(fetchLogs).toHaveBeenCalledWith(500, expect.any(AbortSignal));
    expect(getByText(/Older lines are not shown/)).toBeTruthy();

    fireEvent.click(getByTestId("logs-download"));
    expect(blobs).toHaveLength(1);
    expect(await blobs[0].text()).toBe(CONTENT);
  });

  it.each([
    ["stdout", /stdout/],
    ["missing", /No log file yet/],
  ] as const)("explains an empty %s snapshot and disables Download", async (status, message) => {
    fetchLogs.mockResolvedValue({ status, content: "", lineCount: 0, truncated: false });
    const { findByText, getByTestId } = render(<LogsSettings />);
    await findByText(message);
    expect((getByTestId("logs-download") as HTMLButtonElement).disabled).toBe(true);
  });

  it("says so when the log file is empty", async () => {
    fetchLogs.mockResolvedValue({ status: "available", content: "", lineCount: 0, truncated: false });
    const { findByText } = render(<LogsSettings />);
    await findByText("The log file is empty.");
  });

  it("falls back to a generic message for a non-Error rejection", async () => {
    fetchLogs.mockRejectedValueOnce("boom");
    const { findByText } = render(<LogsSettings />);
    await findByText("Could not load logs");
  });

  it("aborts the in-flight request on unmount", async () => {
    let signal: AbortSignal | undefined;
    fetchLogs.mockImplementation((_lines, s) => {
      signal = s;
      return new Promise(() => {});
    });
    const { unmount } = render(<LogsSettings />);
    await waitFor(() => expect(signal).toBeDefined());
    unmount();
    expect(signal!.aborted).toBe(true);
  });

  it("surfaces a failed fetch and recovers on Refresh", async () => {
    fetchLogs.mockRejectedValueOnce(new Error("Server is in read-only mode"));
    const { findByText, getByTestId, findByTestId } = render(<LogsSettings />);
    await findByText("Server is in read-only mode");

    fetchLogs.mockResolvedValue({ status: "available", content: CONTENT, lineCount: 2, truncated: false });
    await waitFor(() => expect((getByTestId("logs-refresh") as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(getByTestId("logs-refresh"));
    expect((await findByTestId("logs-content")).textContent).toBe(CONTENT);
  });
});

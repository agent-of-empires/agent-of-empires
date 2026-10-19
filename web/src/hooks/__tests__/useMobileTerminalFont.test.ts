// @vitest-environment jsdom
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({ fetchMachineSettings: vi.fn(), updateMobileTerminalFontSize: vi.fn() }));
vi.mock("../../lib/api", () => api);

import { useMobileTerminalFont } from "../useMobileTerminalFont";
import { resetSettingsStore } from "../../lib/settingsStore";

function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
}

beforeEach(() => {
  resetSettingsStore();
  api.fetchMachineSettings.mockReset();
  api.updateMobileTerminalFontSize.mockReset().mockResolvedValue("committed");
});

describe("mobile terminal font configuration", () => {
  it("defaults to 14 and keeps an edit over an older initial config response", async () => {
    const stale = deferred<{ web: { mobile_terminal_font_size: number } }>();
    api.fetchMachineSettings
      .mockReturnValueOnce(stale.promise)
      .mockResolvedValue({ web: { mobile_terminal_font_size: 19 } });
    const hook = renderHook(() => useMobileTerminalFont());
    expect(hook.result.current.fontSize).toBe(14);

    await act(async () => {
      await hook.result.current.update(19);
    });
    expect(hook.result.current.fontSize).toBe(19);
    await act(async () => stale.resolve({ web: { mobile_terminal_font_size: 8 } }));
    await waitFor(() => expect(api.fetchMachineSettings).toHaveBeenCalledTimes(2));
    expect(hook.result.current.fontSize).toBe(19);
  });

  it("shares configuration across readers and saves rapid edits in order", async () => {
    api.fetchMachineSettings.mockResolvedValue({ web: { mobile_terminal_font_size: 23 } });
    const readers = renderHook(() => [useMobileTerminalFont(), useMobileTerminalFont()]);
    await waitFor(() => expect(readers.result.current[0]!.fontSize).toBe(23));
    const firstSave = deferred<"committed">();
    api.updateMobileTerminalFontSize.mockReturnValueOnce(firstSave.promise);
    let first: Promise<boolean>;
    let second: Promise<boolean>;
    act(() => {
      first = readers.result.current[0]!.update(18);
      second = readers.result.current[1]!.update(20);
    });
    await waitFor(() => expect(api.updateMobileTerminalFontSize).toHaveBeenCalledTimes(1));
    expect(readers.result.current.map((reader) => reader.fontSize)).toEqual([20, 20]);
    await act(async () => {
      firstSave.resolve("committed");
      await Promise.all([first!, second!]);
    });
    expect(api.updateMobileTerminalFontSize.mock.calls).toEqual([[18], [20]]);
    expect(readers.result.current.map((reader) => reader.fontSize)).toEqual([20, 20]);
  });

  it("loads configuration again when settings change outside the terminal controls", async () => {
    api.fetchMachineSettings.mockResolvedValue({ web: { mobile_terminal_font_size: 16 } });
    const hook = renderHook(() => useMobileTerminalFont());
    await waitFor(() => expect(hook.result.current.fontSize).toBe(16));
    api.fetchMachineSettings.mockResolvedValue({ web: { mobile_terminal_font_size: 21 } });
    await act(async () => {
      window.dispatchEvent(new Event("aoe:settings-changed"));
    });
    await waitFor(() => expect(hook.result.current.fontSize).toBe(21));
  });
});

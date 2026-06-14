// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";

import { fetchCurrentTheme, fetchResolvedTheme } from "../lib/api";
import { applyResolvedTheme, dispatchThemeChanged, readCachedResolvedTheme, type ResolvedTheme } from "../lib/theme";
import { dispatchThemePickerChanged, THEME_PICKER_CHANGED_EVENT, useResolvedTheme } from "./useResolvedTheme";

vi.mock("../lib/api", () => ({
  fetchCurrentTheme: vi.fn(),
  fetchResolvedTheme: vi.fn(),
}));

vi.mock("../lib/theme", () => ({
  applyResolvedTheme: vi.fn(),
  dispatchThemeChanged: vi.fn(),
  readCachedResolvedTheme: vi.fn(),
}));

const fetchCurrentThemeMock = vi.mocked(fetchCurrentTheme);
const fetchResolvedThemeMock = vi.mocked(fetchResolvedTheme);
const applyResolvedThemeMock = vi.mocked(applyResolvedTheme);
const dispatchThemeChangedMock = vi.mocked(dispatchThemeChanged);
const readCachedResolvedThemeMock = vi.mocked(readCachedResolvedTheme);

function makeTheme(name: string, appearance: "dark" | "light"): ResolvedTheme {
  return {
    name,
    source: "builtin",
    appearance,
    web: { cssVars: {} },
    terminal: { cssVars: {} },
    syntax: { shikiTheme: name },
  };
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.clearAllMocks();
});

beforeEach(() => {
  readCachedResolvedThemeMock.mockReturnValue(null);
});

describe("useResolvedTheme", () => {
  it("seeds initial state from the cached resolved theme", () => {
    const cached = makeTheme("cached-dark", "dark");
    readCachedResolvedThemeMock.mockReturnValue(cached);
    fetchCurrentThemeMock.mockReturnValue(new Promise(() => {})); // never resolves

    const { result } = renderHook(() => useResolvedTheme());
    expect(result.current).toBe(cached);
  });

  it("applies the dark theme fetched on mount", async () => {
    const dark = makeTheme("empire", "dark");
    fetchCurrentThemeMock.mockResolvedValue(dark);

    const { result } = renderHook(() => useResolvedTheme());

    await waitFor(() => expect(result.current).toBe(dark));
    expect(fetchCurrentThemeMock).toHaveBeenCalledTimes(1);
    expect(applyResolvedThemeMock).toHaveBeenCalledWith(dark);
    expect(dispatchThemeChangedMock).toHaveBeenCalledWith(dark);
  });

  it("applies a light theme just as well", async () => {
    const light = makeTheme("daylight", "light");
    fetchCurrentThemeMock.mockResolvedValue(light);

    const { result } = renderHook(() => useResolvedTheme());

    await waitFor(() => expect(result.current?.appearance).toBe("light"));
    expect(applyResolvedThemeMock).toHaveBeenCalledWith(light);
  });

  it("does not apply anything when the mount fetch resolves to null (failure branch)", async () => {
    fetchCurrentThemeMock.mockResolvedValue(null);

    const { result } = renderHook(() => useResolvedTheme());

    await Promise.resolve();
    await Promise.resolve();
    expect(result.current).toBeNull();
    expect(applyResolvedThemeMock).not.toHaveBeenCalled();
    expect(dispatchThemeChangedMock).not.toHaveBeenCalled();
  });

  it("refetches /api/theme/current on a picker event without a name", async () => {
    const initial = makeTheme("empire", "dark");
    const next = makeTheme("forest", "dark");
    fetchCurrentThemeMock.mockResolvedValueOnce(initial).mockResolvedValueOnce(next);

    const { result } = renderHook(() => useResolvedTheme());
    await waitFor(() => expect(result.current).toBe(initial));

    await act(async () => {
      window.dispatchEvent(new CustomEvent(THEME_PICKER_CHANGED_EVENT, { detail: {} }));
      await Promise.resolve();
    });

    await waitFor(() => expect(result.current).toBe(next));
    expect(fetchCurrentThemeMock).toHaveBeenCalledTimes(2);
    expect(fetchResolvedThemeMock).not.toHaveBeenCalled();
  });

  it("refetches /api/themes/:name on a picker event carrying a name", async () => {
    const initial = makeTheme("empire", "dark");
    const named = makeTheme("ocean", "light");
    fetchCurrentThemeMock.mockResolvedValue(initial);
    fetchResolvedThemeMock.mockResolvedValue(named);

    const { result } = renderHook(() => useResolvedTheme());
    await waitFor(() => expect(result.current).toBe(initial));

    await act(async () => {
      dispatchThemePickerChanged("ocean");
      await Promise.resolve();
    });

    await waitFor(() => expect(result.current).toBe(named));
    expect(fetchResolvedThemeMock).toHaveBeenCalledWith("ocean");
  });

  it("ignores a stale fetch that lands after a newer one (sequence guard)", async () => {
    const mountTheme = makeTheme("empire", "dark");
    const pickerTheme = makeTheme("ocean", "light");

    let resolveMount!: (t: ResolvedTheme) => void;
    const slowMount = new Promise<ResolvedTheme>((r) => {
      resolveMount = r;
    });
    fetchCurrentThemeMock.mockReturnValueOnce(slowMount);
    fetchResolvedThemeMock.mockResolvedValue(pickerTheme);

    const { result } = renderHook(() => useResolvedTheme());

    // Picker fetch (seq 2) resolves first and is applied.
    await act(async () => {
      dispatchThemePickerChanged("ocean");
      await Promise.resolve();
    });
    await waitFor(() => expect(result.current).toBe(pickerTheme));

    // Slow mount fetch (seq 1) lands later and must be dropped.
    await act(async () => {
      resolveMount(mountTheme);
      await Promise.resolve();
    });

    expect(result.current).toBe(pickerTheme);
    expect(applyResolvedThemeMock).toHaveBeenCalledTimes(1);
    expect(applyResolvedThemeMock).toHaveBeenLastCalledWith(pickerTheme);
  });

  it("removes the picker listener on unmount so late responses are no-ops", async () => {
    const initial = makeTheme("empire", "dark");
    fetchCurrentThemeMock.mockResolvedValue(initial);

    const removeSpy = vi.spyOn(window, "removeEventListener");
    const { result, unmount } = renderHook(() => useResolvedTheme());
    await waitFor(() => expect(result.current).toBe(initial));

    applyResolvedThemeMock.mockClear();
    unmount();

    expect(removeSpy).toHaveBeenCalledWith(THEME_PICKER_CHANGED_EVENT, expect.any(Function));

    // A picker event after unmount must not trigger any application.
    fetchResolvedThemeMock.mockResolvedValue(makeTheme("ocean", "light"));
    await act(async () => {
      dispatchThemePickerChanged("ocean");
      await Promise.resolve();
    });
    expect(applyResolvedThemeMock).not.toHaveBeenCalled();
  });
});

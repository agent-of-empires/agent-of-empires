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

async function pick(name?: string) {
  await act(async () => {
    dispatchThemePickerChanged(name);
    await Promise.resolve();
  });
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

  it.each([makeTheme("empire", "dark"), makeTheme("daylight", "light")])(
    "applies the $appearance theme fetched on mount",
    async (theme) => {
      fetchCurrentThemeMock.mockResolvedValue(theme);
      const { result } = renderHook(() => useResolvedTheme());
      await waitFor(() => expect(result.current).toBe(theme));
      expect(fetchCurrentThemeMock).toHaveBeenCalledTimes(1);
      expect(applyResolvedThemeMock).toHaveBeenCalledWith(theme);
      expect(dispatchThemeChangedMock).toHaveBeenCalledWith(theme);
    },
  );

  it("does not apply anything when the mount fetch resolves to null", async () => {
    fetchCurrentThemeMock.mockResolvedValue(null);
    const { result } = renderHook(() => useResolvedTheme());

    await Promise.resolve();
    await Promise.resolve();
    expect(result.current).toBeNull();
    expect(applyResolvedThemeMock).not.toHaveBeenCalled();
    expect(dispatchThemeChangedMock).not.toHaveBeenCalled();
  });

  it("a picker event without a name refetches the current theme", async () => {
    const initial = makeTheme("empire", "dark");
    const next = makeTheme("forest", "dark");
    fetchCurrentThemeMock.mockResolvedValueOnce(initial).mockResolvedValueOnce(next);

    const { result } = renderHook(() => useResolvedTheme());
    await waitFor(() => expect(result.current).toBe(initial));
    await pick();

    await waitFor(() => expect(result.current).toBe(next));
    expect(fetchCurrentThemeMock).toHaveBeenCalledTimes(2);
    expect(fetchResolvedThemeMock).not.toHaveBeenCalled();
  });

  it("a picker event carrying a name fetches that theme by name", async () => {
    const initial = makeTheme("empire", "dark");
    const named = makeTheme("ocean", "light");
    fetchCurrentThemeMock.mockResolvedValue(initial);
    fetchResolvedThemeMock.mockResolvedValue(named);

    const { result } = renderHook(() => useResolvedTheme());
    await waitFor(() => expect(result.current).toBe(initial));
    await pick("ocean");

    await waitFor(() => expect(result.current).toBe(named));
    expect(fetchResolvedThemeMock).toHaveBeenCalledWith("ocean");
  });

  it("ignores a stale fetch that lands after a newer one", async () => {
    const mountTheme = makeTheme("empire", "dark");
    const pickerTheme = makeTheme("ocean", "light");
    let resolveMount!: (t: ResolvedTheme) => void;
    fetchCurrentThemeMock.mockReturnValueOnce(
      new Promise<ResolvedTheme>((r) => {
        resolveMount = r;
      }),
    );
    fetchResolvedThemeMock.mockResolvedValue(pickerTheme);

    const { result } = renderHook(() => useResolvedTheme());
    await pick("ocean");
    await waitFor(() => expect(result.current).toBe(pickerTheme));

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

    fetchResolvedThemeMock.mockResolvedValue(makeTheme("ocean", "light"));
    await pick("ocean");
    expect(applyResolvedThemeMock).not.toHaveBeenCalled();
  });
});

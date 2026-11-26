// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, renderHook, screen, waitFor } from "@testing-library/react";

import { fetchCurrentTheme, fetchResolvedTheme } from "../lib/api";
import { getResolvedTheme, setResolvedTheme, THEME_STORAGE_KEY, type ResolvedTheme } from "../lib/theme";
import { dispatchThemePickerChanged, useResolvedTheme } from "./useResolvedTheme";
import { useShikiTheme } from "./useShikiTheme";

vi.mock("../lib/api", () => ({
  fetchCurrentTheme: vi.fn(),
  fetchResolvedTheme: vi.fn(),
}));

const fetchCurrentThemeMock = vi.mocked(fetchCurrentTheme);
const fetchResolvedThemeMock = vi.mocked(fetchResolvedTheme);

function makeTheme(name: string, appearance: "dark" | "light" = "dark"): ResolvedTheme {
  return {
    name,
    source: "builtin",
    appearance,
    web: { cssVars: { "--color-surface-900": `#${name.length}00000` } },
    terminal: { cssVars: {} },
    syntax: { shikiTheme: `${name}-shiki` },
  };
}

async function pick(name?: string) {
  await act(async () => {
    dispatchThemePickerChanged(name);
    await Promise.resolve();
  });
}

const never = () => new Promise<ResolvedTheme | null>(() => {});

beforeEach(() => {
  localStorage.clear();
  setResolvedTheme(makeTheme("start"));
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("useResolvedTheme", () => {
  it("returns the store's theme before the mount fetch lands", () => {
    fetchCurrentThemeMock.mockImplementation(never);
    const { result } = renderHook(() => useResolvedTheme());
    expect(result.current?.name).toBe("start");
  });

  it("applies the theme fetched on mount to <html>", async () => {
    const theme = makeTheme("empire");
    fetchCurrentThemeMock.mockResolvedValue(theme);
    const { result } = renderHook(() => useResolvedTheme());
    await waitFor(() => expect(result.current).toBe(theme));
    expect(document.documentElement.dataset.theme).toBe("empire");
  });

  it("keeps the current theme when the mount fetch resolves to null", async () => {
    fetchCurrentThemeMock.mockResolvedValue(null);
    const { result } = renderHook(() => useResolvedTheme());
    await act(async () => {
      await Promise.resolve();
    });
    expect(result.current?.name).toBe("start");
  });

  it.each([
    ["without a name refetches the current theme", undefined],
    ["with a name fetches that theme", "ocean"],
  ])("a picker event %s", async (_label, name) => {
    const next = makeTheme("ocean", "light");
    fetchCurrentThemeMock.mockResolvedValueOnce(makeTheme("empire")).mockResolvedValue(next);
    fetchResolvedThemeMock.mockResolvedValue(next);
    const { result } = renderHook(() => useResolvedTheme());
    await waitFor(() => expect(result.current?.name).toBe("empire"));
    await pick(name);
    await waitFor(() => expect(result.current).toBe(next));
    if (name) expect(fetchResolvedThemeMock).toHaveBeenCalledWith(name);
    else expect(fetchResolvedThemeMock).not.toHaveBeenCalled();
  });

  it("ignores a stale fetch that lands after a newer one", async () => {
    let resolveMount!: (t: ResolvedTheme) => void;
    fetchCurrentThemeMock.mockReturnValueOnce(new Promise<ResolvedTheme>((r) => (resolveMount = r)));
    const pickerTheme = makeTheme("ocean", "light");
    fetchResolvedThemeMock.mockResolvedValue(pickerTheme);

    const { result } = renderHook(() => useResolvedTheme());
    await pick("ocean");
    await waitFor(() => expect(result.current).toBe(pickerTheme));
    await act(async () => {
      resolveMount(makeTheme("empire"));
      await Promise.resolve();
    });
    expect(result.current).toBe(pickerTheme);
  });

  it("stops listening on unmount", async () => {
    fetchCurrentThemeMock.mockResolvedValue(makeTheme("empire"));
    const { unmount } = renderHook(() => useResolvedTheme());
    await waitFor(() => expect(getResolvedTheme()?.name).toBe("empire"));
    unmount();
    fetchResolvedThemeMock.mockResolvedValue(makeTheme("ocean"));
    await pick("ocean");
    expect(fetchResolvedThemeMock).not.toHaveBeenCalled();
  });

  it("refetches when the window becomes visible again", async () => {
    fetchCurrentThemeMock.mockResolvedValueOnce(makeTheme("empire")).mockResolvedValue(makeTheme("forest"));
    const { result } = renderHook(() => useResolvedTheme());
    await waitFor(() => expect(result.current?.name).toBe("empire"));
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    await waitFor(() => expect(result.current?.name).toBe("forest"));
  });
});

// A pick in one window must repaint every mounted surface of every other window at once, not
// only screens mounted after it (which used to seed themselves from the shared cache).
describe("a theme picked in another window", () => {
  function SyntaxProbe() {
    return <span data-testid="shiki">{useShikiTheme().theme}</span>;
  }

  it("repaints <html> and already-mounted consumers without a remount", async () => {
    fetchCurrentThemeMock.mockImplementation(never);
    renderHook(() => useResolvedTheme());
    render(<SyntaxProbe />);
    expect(screen.getByTestId("shiki").textContent).toBe("start-shiki");

    const picked = makeTheme("dracula");
    act(() => {
      window.dispatchEvent(new StorageEvent("storage", { key: THEME_STORAGE_KEY, newValue: JSON.stringify(picked) }));
    });

    expect(screen.getByTestId("shiki").textContent).toBe("dracula-shiki");
    expect(document.documentElement.dataset.theme).toBe("dracula");
    expect(document.documentElement.style.getPropertyValue("--color-surface-900")).toBe("#700000");
  });

  it("ignores other storage keys and corrupt payloads", () => {
    fetchCurrentThemeMock.mockImplementation(never);
    renderHook(() => useResolvedTheme());
    act(() => {
      window.dispatchEvent(new StorageEvent("storage", { key: "other", newValue: JSON.stringify(makeTheme("x")) }));
      window.dispatchEvent(new StorageEvent("storage", { key: THEME_STORAGE_KEY, newValue: "{bad" }));
    });
    expect(getResolvedTheme()?.name).toBe("start");
  });
});

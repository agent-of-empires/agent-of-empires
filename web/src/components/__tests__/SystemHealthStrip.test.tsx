// @vitest-environment jsdom
//
// Tests for the sidebar system-health strip: the compact readout, its
// pressure banding, the per-agent drill-down, and the setting gate that
// decides whether the strip polls at all.

import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { useState } from "react";
import { SidebarSystemHealth, SystemHealthStrip } from "../SystemHealthStrip";
import { SidebarCompactContext } from "../../lib/sidebarCompact";
import { SystemHealthEnabledContext } from "../../lib/systemHealth";
import type { SystemHealth } from "../../lib/api";

vi.mock("../../lib/api", () => ({
  fetchSystemHealth: vi.fn(),
}));

import { fetchSystemHealth } from "../../lib/api";

const mockFetch = vi.mocked(fetchSystemHealth);

function health(overrides: Partial<SystemHealth> = {}): SystemHealth {
  return {
    status: "ok",
    cpu_fraction: 0.42,
    memory_used_bytes: 8 * 1024 ** 3,
    memory_total_bytes: 32 * 1024 ** 3,
    load_average: [1.5, 1.25, 0.5],
    swap_used_bytes: 0,
    swap_total_bytes: 0,
    agent_count: 2,
    proc_count: 7,
    agents: [],
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

/** The strip is controlled by its parent, which owns the disclosure so the
 *  poll rate can follow it. This harness stands in for that parent. */
function Strip({ health: value }: { health: SystemHealth }) {
  const [expanded, setExpanded] = useState(false);
  return <SystemHealthStrip health={value} expanded={expanded} onToggle={() => setExpanded((v) => !v)} />;
}

describe("SystemHealthStrip", () => {
  it("reports CPU, memory and counts, and hides the detail until opened", () => {
    render(<Strip health={health()} />);

    expect(screen.getByText("CPU 42%")).toBeTruthy();
    expect(screen.getByText("Mem 25%")).toBeTruthy();
    expect(screen.getByText("2 agents · 7 procs")).toBeTruthy();
    expect(screen.queryByTestId("system-health-detail")).toBeNull();
  });

  it("bands the memory readout by the server's status", () => {
    const cases: Array<[SystemHealth["status"], string]> = [
      ["ok", "text-status-running"],
      ["warn", "text-status-warning"],
      ["critical", "text-status-error"],
    ];
    for (const [status, expected] of cases) {
      const { unmount } = render(<Strip health={health({ status })} />);
      expect(screen.getByText(/^Mem /).className).toContain(expected);
      unmount();
    }
  });

  it("reads unknown figures as ? rather than zero", () => {
    render(<Strip health={health({ cpu_fraction: null, memory_total_bytes: 0 })} />);

    expect(screen.getByText("CPU ?")).toBeTruthy();
    expect(screen.getByText("Mem ?")).toBeTruthy();
  });

  it("shows load, swap and the per-agent table when expanded", () => {
    render(
      <Strip
        health={health({
          swap_used_bytes: 512 * 1024 ** 2,
          swap_total_bytes: 2 * 1024 ** 3,
          agents: [
            {
              id: "a1",
              title: "lane-one",
              cpu_fraction: 0.12,
              memory_bytes: 1536 * 1024 ** 2,
              procs: 4,
              sandboxed: false,
            },
            {
              id: "a2",
              title: "lane-two",
              cpu_fraction: null,
              memory_bytes: null,
              procs: null,
              sandboxed: true,
            },
          ],
        })}
      />,
    );

    fireEvent.click(screen.getByTitle("System health"));

    expect(screen.getByText("OK")).toBeTruthy();
    expect(screen.getByText("1.50 / 1.25 / 0.50")).toBeTruthy();
    expect(screen.getByText("512M / 2G")).toBeTruthy();
    expect(screen.getByText("8G / 32G", { exact: false })).toBeTruthy();
    expect(screen.getByText("12%")).toBeTruthy();
    expect(screen.getByText("1.5G")).toBeTruthy();
    // A sandboxed row is marked, and its missing figures read unknown.
    expect(screen.getByText("[container]")).toBeTruthy();
    expect(screen.getAllByText("?").length).toBe(3);
  });

  it("says so when no agents are running", () => {
    render(<Strip health={health({ agent_count: 0, proc_count: 0 })} />);
    fireEvent.click(screen.getByTitle("System health"));

    expect(screen.getByText("No running AoE agents")).toBeTruthy();
  });
});

describe("SidebarSystemHealth", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("renders nothing and fetches nothing while the setting is off", async () => {
    render(
      <SystemHealthEnabledContext.Provider value={false}>
        <SidebarSystemHealth />
      </SystemHealthEnabledContext.Provider>,
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });

    expect(screen.queryByTestId("system-health-strip")).toBeNull();
    expect(mockFetch).not.toHaveBeenCalled();
  });

  it("polls and renders while the setting is on", async () => {
    mockFetch.mockResolvedValue(health());

    render(
      <SystemHealthEnabledContext.Provider value={true}>
        <SidebarSystemHealth />
      </SystemHealthEnabledContext.Provider>,
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(screen.getByTestId("system-health-strip")).toBeTruthy();
    expect(screen.getByText("CPU 42%")).toBeTruthy();

    // Collapsed: every sample costs the host a process scan, and the
    // percentages move slowly, so the closed strip must not poll at the open
    // rate.
    const callsAfterMount = mockFetch.mock.calls.length;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(mockFetch.mock.calls.length).toBe(callsAfterMount);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(12000);
    });
    expect(mockFetch.mock.calls.length).toBeGreaterThan(callsAfterMount);
  });

  it("does not stack requests when a sample outlives the gap", async () => {
    // A slow daemon must not leave a queue of samples behind it: the next
    // request is scheduled from when the last one settles, not on a clock.
    let settle: (value: SystemHealth) => void = () => {};
    mockFetch.mockImplementation(() => new Promise<SystemHealth>((resolve) => (settle = resolve)));

    render(
      <SystemHealthEnabledContext.Provider value={true}>
        <SidebarSystemHealth />
      </SystemHealthEnabledContext.Provider>,
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(mockFetch.mock.calls.length).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(60000);
    });
    expect(mockFetch.mock.calls.length).toBe(1);

    await act(async () => {
      settle(health());
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(mockFetch.mock.calls.length).toBe(1);
  });

  it("does not start a second request when the detail opens mid-sample", async () => {
    // Opening the drill-down restarts the poll at a faster rate; if that
    // restart fired straight away, the host would be sampling twice at once.
    let settle: (value: SystemHealth) => void = () => {};
    mockFetch.mockResolvedValueOnce(health());

    render(
      <SystemHealthEnabledContext.Provider value={true}>
        <SidebarSystemHealth />
      </SystemHealthEnabledContext.Provider>,
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(screen.getByTitle("System health")).toBeTruthy();

    // Second sample is left pending, so a request is in flight when the
    // disclosure opens.
    mockFetch.mockImplementation(() => new Promise<SystemHealth>((resolve) => (settle = resolve)));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(15000);
    });
    const callsWithOneInFlight = mockFetch.mock.calls.length;

    await act(async () => {
      fireEvent.click(screen.getByTitle("System health"));
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(mockFetch.mock.calls.length).toBe(callsWithOneInFlight);

    // Once the pending sample lands the faster loop resumes, and takes over
    // with exactly one request rather than the two it would have had in
    // flight.
    await act(async () => {
      settle(health());
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(mockFetch.mock.calls.length).toBe(callsWithOneInFlight + 1);
  });

  it("stands down in the compact rail rather than cramping the readout", async () => {
    mockFetch.mockResolvedValue(health());

    render(
      <SidebarCompactContext.Provider value={true}>
        <SystemHealthEnabledContext.Provider value={true}>
          <SidebarSystemHealth />
        </SystemHealthEnabledContext.Provider>
      </SidebarCompactContext.Provider>,
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });

    expect(screen.queryByTestId("system-health-strip")).toBeNull();
    expect(mockFetch).not.toHaveBeenCalled();
  });

  it("polls at the TUI's rate only while the detail is open", async () => {
    mockFetch.mockResolvedValue(health());

    render(
      <SystemHealthEnabledContext.Provider value={true}>
        <SidebarSystemHealth />
      </SystemHealthEnabledContext.Provider>,
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    fireEvent.click(screen.getByTitle("System health"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    const callsAfterOpen = mockFetch.mock.calls.length;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(600);
    });
    expect(mockFetch.mock.calls.length).toBeGreaterThan(callsAfterOpen);
  });

  it("stops polling while the tab is hidden and refreshes on return", async () => {
    mockFetch.mockResolvedValue(health());

    render(
      <SystemHealthEnabledContext.Provider value={true}>
        <SidebarSystemHealth />
      </SystemHealthEnabledContext.Provider>,
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    const hidden = vi.spyOn(document, "hidden", "get").mockReturnValue(true);
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(0);
    });

    const callsWhileHidden = mockFetch.mock.calls.length;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(16000);
    });
    expect(mockFetch.mock.calls.length).toBe(callsWhileHidden);

    // Returning to the foreground reads once immediately: a reading from a
    // minute ago is worse than no reading when you have just looked back.
    hidden.mockReturnValue(false);
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(mockFetch.mock.calls.length).toBeGreaterThan(callsWhileHidden);
  });
});

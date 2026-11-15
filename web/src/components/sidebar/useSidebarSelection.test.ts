// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { MouseEvent as ReactMouseEvent } from "react";

import type { useSidebarTriage } from "../../hooks/useSidebarTriage";
import { EMPTY_OPTIMISTIC } from "../../lib/sidebarOptimistic";
import { useSidebarSelection } from "./useSidebarSelection";

const triage = {
  optimisticFor: () => EMPTY_OPTIMISTIC,
  bulkPin: vi.fn(),
  bulkArchive: vi.fn(),
  bulkSnooze: vi.fn(),
} as unknown as ReturnType<typeof useSidebarTriage>;

const navigateClick = () => new MouseEvent("click", { bubbles: true }) as unknown as ReactMouseEvent;

describe("useSidebarSelection active hint", () => {
  it("drops the hint when another route opens a different session of the same workspace", () => {
    const onSelect = vi.fn();
    const base = { rows: [], orderedIds: [], triage, activeId: "w", onSelect };
    const { result, rerender } = renderHook(
      ({ activeSessionId }: { activeSessionId: string | null }) => useSidebarSelection({ ...base, activeSessionId }),
      { initialProps: { activeSessionId: "a" as string | null } },
    );

    // The click shows the target row before the route catches up.
    act(() => result.current.handleRowActivate("row-b", navigateClick(), "b"));
    expect(result.current.displayedActiveSessionId).toBe("b");

    // Another route then opens a third session of the same workspace: only the pair can tell,
    // so the stale hint must not keep the other row highlighted.
    rerender({ activeSessionId: "c" });
    expect(result.current.displayedActiveSessionId).toBe("c");
  });
});

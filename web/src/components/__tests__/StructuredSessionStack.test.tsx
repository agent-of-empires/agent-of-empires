// @vitest-environment jsdom

import { useEffect } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { StructuredSessionStack } from "../StructuredSessionStack";
import { TOUR_ANCHORS, tourAnchor, tourSelector } from "../../lib/tourSteps";
import { makeSession as baseSession } from "./fixtures";

afterEach(cleanup);

const makeSession = (id: string) => baseSession({ id, title: id, view: "structured" });
const sessions = ["s1", "s2", "s3"].map(makeSession);

function setup(initial: { id: string; persistent: boolean; limit: number; stackActive?: boolean }) {
  const mounts: Record<string, number> = {};
  const live = new Set<string>();
  let maxLive = 0;
  const Probe = ({ id, active, selected }: { id: string; active: boolean; selected: boolean }) => {
    useEffect(() => {
      mounts[id] = (mounts[id] ?? 0) + 1;
      live.add(id);
      maxLive = Math.max(maxLive, live.size);
      return () => void live.delete(id);
    }, [id]);
    return (
      <div
        {...tourAnchor(TOUR_ANCHORS.composer)}
        data-testid={`view-${id}`}
        data-active={String(active)}
        data-selected={String(selected)}
      />
    );
  };
  const ui = (p: typeof initial) => (
    <StructuredSessionStack
      active={p.stackActive}
      activeSessionId={p.id}
      sessions={sessions}
      persistent={p.persistent}
      maxPersistentViews={p.limit}
      renderSession={(session, active, selected) => <Probe id={session.id} active={active} selected={selected} />}
    />
  );
  const { rerender } = render(ui(initial));
  return {
    mounts,
    maxLive: () => maxLive,
    // Lets the previous active session reach the recent list before the next switch.
    show: async (p: typeof initial) => {
      await act(async () => {});
      rerender(ui(p));
    },
  };
}

describe("StructuredSessionStack", () => {
  it("mounts only the active session when persistence is off", async () => {
    const { show } = setup({ id: "s1", persistent: false, limit: 3 });
    await show({ id: "s2", persistent: false, limit: 3 });

    expect(screen.queryByTestId("view-s1")).toBeNull();
    expect(screen.getByTestId("view-s2").dataset.active).toBe("true");
  });

  it("keeps recent sessions mounted, hidden and inert, and revisits without remounting", async () => {
    const { show, mounts } = setup({ id: "s1", persistent: true, limit: 3 });
    await show({ id: "s2", persistent: true, limit: 3 });
    await waitFor(() => expect(screen.getByTestId("view-s1")).toBeDefined());

    const hidden = screen.getByTestId("view-s1").parentElement!;
    expect(hidden.getAttribute("aria-hidden")).toBe("true");
    expect(hidden.hasAttribute("inert")).toBe(true);
    expect(screen.getByTestId("view-s1").dataset.active).toBe("false");

    await show({ id: "s1", persistent: true, limit: 3 });
    await waitFor(() => expect(screen.getByTestId("view-s1").dataset.active).toBe("true"));
    expect(mounts).toEqual({ s1: 1, s2: 1 });
  });

  it("never detaches a kept view when switching, which would reset its scroll position", async () => {
    const { show } = setup({ id: "s1", persistent: true, limit: 3 });
    await show({ id: "s2", persistent: true, limit: 3 });
    await waitFor(() => expect(screen.getByTestId("view-s1")).toBeDefined());
    await show({ id: "s3", persistent: true, limit: 3 });
    await waitFor(() => expect(screen.getByTestId("view-s2")).toBeDefined());

    const stack = screen.getByTestId("view-s1").parentElement!.parentElement!;
    const detached: Node[] = [];
    const record = (records: MutationRecord[]) => detached.push(...records.flatMap((r) => [...r.removedNodes]));
    const observer = new MutationObserver(record);
    observer.observe(stack, { childList: true });
    for (const id of ["s1", "s2", "s3", "s1"]) {
      await show({ id, persistent: true, limit: 3 });
      await waitFor(() => expect(screen.getByTestId(`view-${id}`).dataset.active).toBe("true"));
      await act(async () => {});
    }

    record(observer.takeRecords());
    observer.disconnect();
    expect(detached).toEqual([]);
  });

  it("resolves a tour anchor to the selected view when a kept view comes first in the DOM", async () => {
    const { show } = setup({ id: "s1", persistent: true, limit: 3 });
    await show({ id: "s2", persistent: true, limit: 3 });
    await waitFor(() => expect(screen.getByTestId("view-s1")).toBeDefined());

    expect(document.querySelector(tourSelector(TOUR_ANCHORS.composer))).toBe(screen.getByTestId("view-s2"));
  });

  it("never mounts more than the limit, evicting the least recent", async () => {
    const { show, maxLive } = setup({ id: "s1", persistent: true, limit: 2 });
    await show({ id: "s2", persistent: true, limit: 2 });
    await waitFor(() => expect(screen.getByTestId("view-s1")).toBeDefined());
    await show({ id: "s3", persistent: true, limit: 2 });

    await waitFor(() => expect(screen.queryByTestId("view-s1")).toBeNull());
    expect(screen.getByTestId("view-s2").dataset.active).toBe("false");
    expect(screen.getByTestId("view-s3").dataset.active).toBe("true");
    expect(maxLive()).toBe(2);
  });

  it("marks every view inactive while the stack is covered, but the selected one stays selected", async () => {
    const { show } = setup({ id: "s1", persistent: true, limit: 3 });
    await show({ id: "s2", persistent: true, limit: 3 });
    await waitFor(() => expect(screen.getByTestId("view-s1")).toBeDefined());
    await show({ id: "s2", persistent: true, limit: 3, stackActive: false });

    expect(screen.getByTestId("view-s2").dataset.active).toBe("false");
    expect(screen.getByTestId("view-s2").dataset.selected).toBe("true");
    expect(screen.getByTestId("view-s1").dataset.selected).toBe("false");
  });
});

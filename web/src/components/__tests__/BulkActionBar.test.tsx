// @vitest-environment jsdom
//
// Coverage for BulkActionBar: hidden at zero selection, count-labelled
// per-eligibility buttons (Pin/Unpin/Archive/Unarchive/Snooze/Unsnooze) that
// each apply only to their compatible subset (#1724), the snooze preset menu,
// and Clear.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { BulkActionBar } from "../BulkActionBar";
import type { BulkTriageBuckets } from "../../lib/sidebarBulk";
import type { Workspace } from "../../lib/types";

const ws = (id: string) => ({ id }) as Workspace;

function buckets(over: Partial<BulkTriageBuckets> = {}): BulkTriageBuckets {
  return {
    pinnable: [],
    archivable: [],
    snoozable: [],
    unpinnable: [],
    unarchivable: [],
    unsnoozable: [],
    ...over,
  };
}

const noop = {
  onBulkPin: () => {},
  onBulkArchive: () => {},
  onBulkSnooze: () => {},
  onClear: () => {},
};

afterEach(cleanup);

describe("BulkActionBar", () => {
  it("renders nothing when nothing is selected", () => {
    const { container } = render(<BulkActionBar selectedCount={0} buckets={buckets()} snoozePresets={[]} {...noop} />);
    expect(container.firstChild).toBeNull();
  });

  it("shows count-labelled pin/archive buttons for the eligible subset", () => {
    const onBulkPin = vi.fn();
    const onBulkArchive = vi.fn();
    render(
      <BulkActionBar
        selectedCount={3}
        buckets={buckets({ pinnable: [ws("a"), ws("b")], archivable: [ws("c")] })}
        snoozePresets={[]}
        {...noop}
        onBulkPin={onBulkPin}
        onBulkArchive={onBulkArchive}
      />,
    );
    expect(screen.getByText("3 selected")).toBeTruthy();

    fireEvent.click(screen.getByTestId("sidebar-bulk-pin"));
    expect(onBulkPin).toHaveBeenCalledWith([ws("a"), ws("b")], true);

    fireEvent.click(screen.getByTestId("sidebar-bulk-archive"));
    expect(onBulkArchive).toHaveBeenCalledWith([ws("c")], true);
  });

  it("shows inverse buttons for already-triaged rows", () => {
    const onBulkPin = vi.fn();
    const onBulkSnooze = vi.fn();
    render(
      <BulkActionBar
        selectedCount={2}
        buckets={buckets({ unpinnable: [ws("a")], unsnoozable: [ws("b")] })}
        snoozePresets={[]}
        {...noop}
        onBulkPin={onBulkPin}
        onBulkSnooze={onBulkSnooze}
      />,
    );
    fireEvent.click(screen.getByTestId("sidebar-bulk-unpin"));
    expect(onBulkPin).toHaveBeenCalledWith([ws("a")], false);
    fireEvent.click(screen.getByTestId("sidebar-bulk-unsnooze"));
    expect(onBulkSnooze).toHaveBeenCalledWith([ws("b")], null);
  });

  it("opens the snooze menu and applies a preset", () => {
    const onBulkSnooze = vi.fn();
    render(
      <BulkActionBar
        selectedCount={1}
        buckets={buckets({ snoozable: [ws("a")] })}
        snoozePresets={[{ label: "1h", minutes: 60 }]}
        {...noop}
        onBulkSnooze={onBulkSnooze}
      />,
    );
    fireEvent.click(screen.getByTestId("sidebar-bulk-snooze"));
    fireEvent.click(screen.getByRole("menuitem", { name: "1h" }));
    expect(onBulkSnooze).toHaveBeenCalledWith([ws("a")], 60);
    expect(screen.queryByTestId("sidebar-bulk-snooze-menu")).toBeNull();
  });

  it("fires onClear", () => {
    const onClear = vi.fn();
    render(<BulkActionBar selectedCount={1} buckets={buckets()} snoozePresets={[]} {...noop} onClear={onClear} />);
    fireEvent.click(screen.getByLabelText("Clear selection"));
    expect(onClear).toHaveBeenCalledOnce();
  });
});

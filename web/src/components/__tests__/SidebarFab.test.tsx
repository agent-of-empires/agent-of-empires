// @vitest-environment jsdom
//
// Unit tests for the mobile sidebar-toggle FAB (#2245). It mirrors the
// keyboard FAB: an aria-label that flips with sidebar state and an
// onMouseDown preventDefault so tapping it does not blur the terminal input
// (which would drop the soft keyboard before the sidebar toggles).

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SidebarFab } from "../SidebarFab";

afterEach(() => {
  cleanup();
});

describe("SidebarFab", () => {
  it("labels itself 'Open sidebar' when the sidebar is closed", () => {
    render(<SidebarFab sidebarOpen={false} onToggle={vi.fn()} />);
    expect(screen.getByLabelText("Open sidebar")).toBeTruthy();
  });

  it("labels itself 'Close sidebar' when the sidebar is open", () => {
    render(<SidebarFab sidebarOpen={true} onToggle={vi.fn()} />);
    expect(screen.getByLabelText("Close sidebar")).toBeTruthy();
  });

  it("fires onToggle when clicked", () => {
    const onToggle = vi.fn();
    render(<SidebarFab sidebarOpen={false} onToggle={onToggle} />);
    fireEvent.click(screen.getByLabelText("Open sidebar"));
    expect(onToggle).toHaveBeenCalledTimes(1);
  });

  it("prevents the default on mousedown so it does not steal focus from the terminal", () => {
    render(<SidebarFab sidebarOpen={false} onToggle={vi.fn()} />);
    const button = screen.getByLabelText("Open sidebar");
    const event = new MouseEvent("mousedown", { bubbles: true, cancelable: true });
    const prevented = !button.dispatchEvent(event);
    expect(prevented).toBe(true);
  });
});

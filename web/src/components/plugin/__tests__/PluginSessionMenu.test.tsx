// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { makeSession, makeWorkspace, stubFetch } from "../../__tests__/fixtures";
import { PluginSessionMenu, type SessionMenuRequest } from "../PluginSessionMenu";

const workspaces = [makeWorkspace("w1", [makeSession({ id: "s1", title: "Ethiopians" })])];

function open(over: Partial<SessionMenuRequest> = {}) {
  const onClose = vi.fn();
  const request: SessionMenuRequest = { key: 1, sessionId: "s1", x: 40, y: 50, items: [], onClose, ...over };
  render(<PluginSessionMenu request={request} workspaces={workspaces} readOnly={false} handlers={{}} />);
  return onClose;
}

const backdrop = () => screen.getByTestId("plugin-session-menu-backdrop");
const arm = () => act(() => vi.advanceTimersByTime(300));

let fetchSpy: ReturnType<typeof stubFetch>;
beforeEach(() => {
  vi.useFakeTimers();
  fetchSpy = stubFetch();
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("PluginSessionMenu", () => {
  it("shields the menu until armed, then a page item resolves with its id", () => {
    const onClose = open({ items: [{ id: "move:done", label: "Move to Done" }] });
    expect(screen.getByTestId("sidebar-context-menu")).toBeTruthy();
    expect(backdrop().className).toContain("z-[60]");
    fireEvent.mouseDown(backdrop());
    fireEvent.contextMenu(backdrop());
    expect(screen.getByTestId("sidebar-context-menu")).toBeTruthy();

    arm();
    expect(backdrop().className).toContain("z-40");
    fireEvent.click(screen.getByRole("button", { name: "Move to Done" }));
    expect(screen.queryByTestId("sidebar-context-menu")).toBeNull();
    expect(onClose).toHaveBeenCalledExactlyOnceWith("move:done");
  });

  it("an armed backdrop click closes with nothing picked", () => {
    const onClose = open();
    arm();
    fireEvent.mouseDown(backdrop());
    expect(screen.queryByTestId("sidebar-context-menu")).toBeNull();
    expect(onClose).toHaveBeenCalledExactlyOnceWith(null);
  });

  it("resolves null at once for a session the dashboard does not know", () => {
    const onClose = open({ sessionId: "missing" });
    expect(onClose).toHaveBeenCalledExactlyOnceWith(null);
    expect(screen.queryByTestId("sidebar-context-menu")).toBeNull();
  });

  it("renames through a dialog, since the row's inline rename is not here", async () => {
    const onClose = open();
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-rename"));
    expect(onClose).toHaveBeenCalledExactlyOnceWith(null);
    const input = screen.getByTestId("rename-modal-input") as HTMLInputElement;
    expect(input.value).toBe("Ethiopians");
    fireEvent.change(input, { target: { value: "Britons" } });
    vi.useRealTimers();
    fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    await waitFor(() => expect(screen.queryByTestId("rename-modal")).toBeNull());
    const [url, init] = fetchSpy.mock.calls[0];
    expect(String(url)).toContain("/api/sessions/s1");
    expect(init).toMatchObject({ method: "PATCH", body: JSON.stringify({ title: "Britons" }) });
  });

  it("opens the snooze picker from the menu", () => {
    open();
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-snooze"));
    expect(screen.getByTestId("snooze-modal")).toBeTruthy();
  });
});

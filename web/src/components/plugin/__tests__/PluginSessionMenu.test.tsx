// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { makeSession, makeWorkspace, stubFetch } from "../../__tests__/fixtures";
import { PluginSessionMenu, type SessionMenuRequest } from "../PluginSessionMenu";

const workspaces = [makeWorkspace("w1", [makeSession({ id: "s1", title: "Ethiopians" })])];

const handlers = { onStop: vi.fn(), onDelete: vi.fn() };

function open(over: Partial<SessionMenuRequest> = {}) {
  const onClose = vi.fn();
  const request: SessionMenuRequest = { key: 1, sessionId: "s1", x: 40, y: 50, items: [], onClose, ...over };
  render(<PluginSessionMenu request={request} workspaces={workspaces} readOnly={false} handlers={handlers} />);
  return onClose;
}

const backdrop = () => screen.getByTestId("plugin-session-menu-backdrop");
const arm = () => act(() => vi.advanceTimersByTime(300));

let fetchSpy: ReturnType<typeof stubFetch>;
beforeEach(() => {
  vi.useFakeTimers();
  fetchSpy = stubFetch();
  handlers.onStop.mockReset();
  handlers.onDelete.mockReset();
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
    expect(backdrop().dataset.armed).toBeUndefined();
    fireEvent.mouseDown(backdrop());
    fireEvent.contextMenu(backdrop());
    expect(screen.getByTestId("sidebar-context-menu")).toBeTruthy();

    arm();
    expect(backdrop().className).toContain("z-40");
    expect(backdrop().dataset.armed).toBe("true");
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

  it("snoozes the workspace with a picked preset", async () => {
    open();
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-snooze"));
    vi.useRealTimers();
    fireEvent.click(screen.getByRole("button", { name: "1 hour" }));
    expect(screen.queryByTestId("snooze-modal")).toBeNull();
    await waitFor(() => expect(String(fetchSpy.mock.calls[0]?.[0])).toContain("/api/sessions/s1/snooze"));
  });

  it.each([
    { item: "sidebar-context-menu-stop", called: () => handlers.onStop, args: ["s1"] },
    { item: "sidebar-context-menu-delete", called: () => handlers.onDelete, args: [["s1"]] },
  ])("$item calls back with the session", ({ item, called, args }) => {
    const onClose = open();
    arm();
    fireEvent.click(screen.getByTestId(item));
    expect(called()).toHaveBeenCalledExactlyOnceWith(...args);
    expect(onClose).toHaveBeenCalledExactlyOnceWith(null);
  });

  it.each([
    ["sidebar-context-menu-archive", "/api/sessions/s1/archive"],
    ["sidebar-context-menu-pin", "/api/sessions/s1/pin"],
  ])("%s goes through the optimistic triage", async (item, path) => {
    open();
    arm();
    vi.useRealTimers();
    fireEvent.click(screen.getByTestId(item));
    await waitFor(() =>
      expect(fetchSpy.mock.calls.map(([url]) => String(url))).toContainEqual(expect.stringContaining(path)),
    );
  });

  it("opens the group picker from More", () => {
    open();
    arm();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-more"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-edit-group"));
    expect(screen.getByTestId("session-group-modal-input")).toBeTruthy();
  });
});

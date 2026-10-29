// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";

import { toastBus } from "../../lib/toastBus";
import { jsonResponse, makeSession, makeWorkspace, openRowMenu } from "./fixtures";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  toastBus.handler = null;
});

describe("Recreate container", () => {
  it("is offered only for a sandboxed session", () => {
    openRowMenu(makeWorkspace("w", [makeSession({ id: "s1" })]));
    expect(screen.queryByTestId("sidebar-context-menu-recreate-container")).toBeNull();
  });

  it.each([
    ["recreates and closes", jsonResponse({}, 200), null],
    [
      "stays open on a refusal and says why",
      jsonResponse({ error: "lifecycle_busy", message: "Session is busy" }, 409),
      "Session is busy",
    ],
  ])("%s", async (_label, response, refusal) => {
    const fetchSpy = vi.fn<typeof fetch>(async () => response);
    vi.stubGlobal("fetch", fetchSpy);
    const info = vi.fn();
    toastBus.handler = { error: vi.fn(), info, push: vi.fn(), openLink: vi.fn() };
    openRowMenu(makeWorkspace("w", [makeSession({ id: "s1", title: "boxed", is_sandboxed: true })]));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-recreate-container"));

    const dialog = screen.getByRole("dialog", { name: "Recreate container" });
    expect(dialog.textContent).toContain(
      "Files outside the Working directory and the Sandbox's mounted folders are lost, including anything installed inside the container.",
    );
    expect(dialog.textContent).toContain("a running turn stops");
    expect(fetchSpy).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("recreate-container-confirm"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalledOnce());
    expect(fetchSpy.mock.calls[0]![0]).toBe("/api/sessions/s1/recreate-container");
    expect(fetchSpy.mock.calls[0]![1]?.method).toBe("POST");
    if (refusal) {
      expect((await screen.findByRole("alert")).textContent).toBe(`Couldn't recreate the container: ${refusal}`);
      expect(screen.getByRole("dialog", { name: "Recreate container" })).toBeTruthy();
      expect(info).not.toHaveBeenCalled();
    } else {
      await vi.waitFor(() => expect(screen.queryByRole("dialog", { name: "Recreate container" })).toBeNull());
      expect(info).toHaveBeenCalledWith("Restarting boxed on a new container…");
    }
  });
});

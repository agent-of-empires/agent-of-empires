// @vitest-environment jsdom
// The Session menu's managed-session actions dispatch to the triage hook.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";
import { firstRequest, makeSession, makeWorkspace, openRowMenu, stubFetch } from "./fixtures";

const PAST = "2026-01-01T00:00:00Z";
const managed = (over: Partial<SessionResponse> = {}) =>
  makeWorkspace("w", [makeSession({ parent_session_id: "manager", ...over })]);

let fetchSpy: ReturnType<typeof stubFetch>;
beforeEach(() => {
  fetchSpy = stubFetch();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("Session menu managed-session actions", () => {
  it("ignores a managed row through the PATCH endpoint", async () => {
    openRowMenu(managed());
    fireEvent.click(screen.getByRole("button", { name: "Ignore managed session" }));
    await waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    expect(firstRequest(fetchSpy)).toEqual({
      url: "/api/sessions/s1/ignored",
      method: "PATCH",
      body: { ignored: true },
    });
  });

  it("unignores an ignored row through the same endpoint", async () => {
    openRowMenu(managed({ ignored_at: PAST }));
    fireEvent.click(screen.getByRole("button", { name: "Unignore session" }));
    await waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    expect(firstRequest(fetchSpy)).toEqual({
      url: "/api/sessions/s1/ignored",
      method: "PATCH",
      body: { ignored: false },
    });
  });

  it("labels each sibling in the picker and toggles only the one picked", async () => {
    const ws = makeWorkspace("w", [
      makeSession({ id: "a", title: "alpha", parent_session_id: "manager" }),
      makeSession({ id: "b", title: "beta", parent_session_id: "manager", ignored_at: PAST }),
    ]);
    // (picker index, its own label, the flag the PATCH must carry)
    const cases: [number, string, boolean][] = [
      [0, "not ignored", true],
      [1, "ignored", false],
    ];
    for (const [index, label, ignored] of cases) {
      openRowMenu(ws);
      fireEvent.click(screen.getByRole("button", { name: "Ignore managed session…" }));
      const targets = screen.getAllByTestId("sidebar-context-menu-ignore-target");
      expect(targets.map((target) => within(target).getByText(/^(not )?ignored$/).textContent)).toEqual([
        "not ignored",
        "ignored",
      ]);
      fireEvent.click(targets[index]!);
      await waitFor(() => expect(fetchSpy).toHaveBeenCalled());
      expect(firstRequest(fetchSpy), label).toEqual({
        url: `/api/sessions/${index === 0 ? "a" : "b"}/ignored`,
        method: "PATCH",
        body: { ignored },
      });
      cleanup();
      fetchSpy.mockClear();
    }
  });

  it("reclaims ownership of a managed row", async () => {
    openRowMenu(managed());
    fireEvent.click(screen.getByRole("button", { name: "Take ownership back" }));
    await waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    expect(firstRequest(fetchSpy)).toEqual({
      url: "/api/sessions/s1/reclaim-ownership",
      method: "POST",
      body: { reclaim: true },
    });
  });
});

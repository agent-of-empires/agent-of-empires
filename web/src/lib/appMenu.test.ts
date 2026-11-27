import { describe, expect, it, vi } from "vitest";

import { appMenuItems } from "./appMenu";

describe("appMenuItems", () => {
  it("lists the app's items, with Sign out only where sign-in is required, each wired to its handler", () => {
    for (const loginRequired of [false, true]) {
      const h = {
        onOpenHelp: vi.fn(),
        onOpenLegend: vi.fn(),
        onStartTutorial: vi.fn(),
        onOpenTips: vi.fn(),
        onOpenAbout: vi.fn(),
        onLogout: vi.fn(),
        loginRequired,
      };
      const items = appMenuItems(h);
      expect(items.map((i) => i.label)).toEqual([
        "Help",
        "Symbol legend",
        "Show tutorial",
        "Tips",
        "About",
        ...(loginRequired ? ["Sign out"] : []),
      ]);
      for (const item of items) item.onClick();
      const fired = [h.onOpenHelp, h.onOpenLegend, h.onStartTutorial, h.onOpenTips, h.onOpenAbout, h.onLogout];
      expect(fired.map((f) => f.mock.calls.length)).toEqual([1, 1, 1, 1, 1, loginRequired ? 1 : 0]);
    }
  });
});

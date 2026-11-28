// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { focusSessionSidebar } from "../focusSessionSidebar";

let frames: FrameRequestCallback[] = [];
const runFrames = (count: number) => {
  for (let i = 0; i < count; i += 1) {
    const due = frames;
    frames = [];
    due.forEach((cb) => cb(0));
  }
};

/** A phone's drawer cannot take focus until it has rendered open: `focus()` does nothing before `readyAt` frames. */
function mountSidebar(readyAt: number, { withRow = true } = {}) {
  document.body.innerHTML = `<nav role="navigation" aria-label="Sessions sidebar" tabindex="-1">${
    withRow ? '<a href="/session/a" data-active-session-row>row</a>' : ""
  }</nav>`;
  const sidebar = document.querySelector<HTMLElement>("nav")!;
  const row = document.querySelector<HTMLElement>("[data-active-session-row]");
  let elapsed = 0;
  vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb) => {
    frames.push((t) => {
      elapsed += 1;
      cb(t);
    });
    return frames.length;
  });
  for (const el of [sidebar, row]) {
    if (!el) continue;
    const focus = HTMLElement.prototype.focus;
    el.focus = () => {
      if (elapsed >= readyAt) focus.call(el);
    };
    el.getClientRects = () => (elapsed >= readyAt ? ([{}] as unknown as DOMRectList) : ([] as unknown as DOMRectList));
  }
  return { sidebar, row };
}

beforeEach(() => {
  frames = [];
});
afterEach(() => {
  vi.restoreAllMocks();
  document.body.innerHTML = "";
});

describe("focusSessionSidebar", () => {
  it("focuses the active row once the drawer is ready", () => {
    const { row } = mountSidebar(1);
    focusSessionSidebar();
    runFrames(3);
    expect(document.activeElement).toBe(row);
  });

  it("keeps trying while the drawer is still opening", () => {
    const { row } = mountSidebar(6);
    focusSessionSidebar();
    runFrames(14);
    expect(document.activeElement).toBe(row);
  });

  it("focuses the sidebar when no session row is active", () => {
    const { sidebar } = mountSidebar(4, { withRow: false });
    focusSessionSidebar();
    runFrames(14);
    expect(document.activeElement).toBe(sidebar);
  });

  it("gives up after a bounded number of frames", () => {
    const { sidebar, row } = mountSidebar(1000);
    focusSessionSidebar();
    runFrames(200);
    expect(document.activeElement).not.toBe(row);
    expect(document.activeElement).not.toBe(sidebar);
    expect(frames).toEqual([]);
  });
});

// @vitest-environment jsdom
// Keyboard opening and Tab order need a real browser: tests/wizard-worktree-info.spec.ts.
import { useRef } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { InfoPopover } from "../InfoPopover";
import { useFocusTrap } from "../../../hooks/useFocusTrap";

afterEach(cleanup);

function renderPopover() {
  render(
    <>
      <InfoPopover title="What's a worktree?">
        <a href="/settings/worktree">Worktree settings ›</a>
      </InfoPopover>
      <p>outside</p>
      <button type="button">wizard field</button>
    </>,
  );
  return screen.getByRole("button", { name: "What's a worktree?" });
}

const dialog = () => screen.queryByRole("dialog", { name: "What's a worktree?" });

function TrappedModal() {
  const ref = useRef<HTMLDivElement>(null);
  useFocusTrap(() => ref.current);
  return (
    <div ref={ref} role="dialog" aria-modal="true" aria-label="New session">
      <button type="button">first field</button>
      <InfoPopover title="What's a worktree?">
        <a href="/settings/worktree">Worktree settings ›</a>
      </InfoPopover>
    </div>
  );
}

describe("InfoPopover", () => {
  it("closes when focus moves outside it, leaving focus there, and stays open for the link", () => {
    fireEvent.click(renderPopover());
    screen.getByRole("link", { name: "Worktree settings ›" }).focus();
    expect(dialog()).not.toBeNull();
    const field = screen.getByRole("button", { name: "wizard field" });
    act(() => field.focus());
    expect(dialog()).toBeNull();
    expect(document.activeElement).toBe(field);
  });

  // The wizard traps Tab: a panel outside its root would send Tab back to the first field and close.
  it("stays inside a modal's focus trap, so Tab goes on from it", () => {
    render(<TrappedModal />);
    fireEvent.click(screen.getByRole("button", { name: "What's a worktree?" }));
    expect(document.activeElement).toBe(dialog());
    expect(screen.getByRole("dialog", { name: "New session" }).contains(dialog())).toBe(true);
    // Not cancelled: the browser moves on to the link.
    expect(fireEvent.keyDown(dialog()!, { key: "Tab" })).toBe(true);
    expect(dialog()).not.toBeNull();
    expect(document.activeElement).toBe(dialog());
  });

  it("opens a dialog that takes focus, and a second click closes it", () => {
    const icon = renderPopover();
    expect(icon.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(icon);
    expect(dialog()).not.toBeNull();
    expect(document.activeElement).toBe(dialog());
    expect(icon.getAttribute("aria-expanded")).toBe("true");
    expect(icon.getAttribute("aria-controls")).toBe(dialog()!.id);
    fireEvent.click(icon);
    expect(dialog()).toBeNull();
  });

  // That the wizard around it stays open is checked in the browser spec, with the app's shortcuts.
  it("closes on Escape and refocuses the icon", () => {
    const icon = renderPopover();
    fireEvent.click(icon);
    fireEvent.keyDown(screen.getByRole("link", { name: "Worktree settings ›" }), { key: "Escape" });
    expect(dialog()).toBeNull();
    expect(document.activeElement).toBe(icon);
  });

  it("closes on a press outside without taking focus back, and stays open for one inside", () => {
    const icon = renderPopover();
    fireEvent.click(icon);
    fireEvent.pointerDown(screen.getByRole("link", { name: "Worktree settings ›" }));
    expect(dialog()).not.toBeNull();
    fireEvent.pointerDown(screen.getByText("outside"));
    expect(dialog()).toBeNull();
    expect(document.activeElement).not.toBe(icon);
  });

  const resize = (width: number, height: number) => () => {
    Object.assign(window, { innerWidth: width, innerHeight: height });
    fireEvent(window, new Event("resize"));
  };
  const scroll = () => fireEvent.scroll(screen.getByText("outside"));
  // A narrower window moves the icon but not the panel's size, so nothing else would re-place it. A phone's
  // keyboard and address bar change only the height, and opening the popover can close the keyboard.
  // Closing with focus inside returns it to the icon, so Tab keeps its place; focus elsewhere stays put.
  it.each([
    ["closes when the page under it scrolls, focus inside", scroll, "inside", "icon"],
    ["closes when the page under it scrolls, focus elsewhere", scroll, "elsewhere", "body"],
    ["closes when the window's width changes, focus inside", resize(375, 768), "inside", "icon"],
    ["closes when the window's width changes, focus elsewhere", resize(375, 768), "elsewhere", "body"],
    ["stays open when only the window's height changes", resize(1024, 400), "inside", "open"],
  ] as const)("%s", (_label, change, focus, expected) => {
    Object.assign(window, { innerWidth: 1024, innerHeight: 768 });
    const icon = renderPopover();
    fireEvent.click(icon);
    const linkEl = screen.getByRole("link", { name: "Worktree settings ›" });
    linkEl.focus();
    // Focus leaving for nowhere (a removed element, a click on the page) fires no focusin, so the popover stays.
    if (focus === "elsewhere") linkEl.blur();
    change();
    if (expected === "open") {
      expect(dialog()).not.toBeNull();
      expect(document.activeElement).toBe(linkEl);
    } else {
      expect(dialog()).toBeNull();
      expect(document.activeElement).toBe(expected === "icon" ? icon : document.body);
    }
    Object.assign(window, { innerWidth: 1024, innerHeight: 768 });
  });
});

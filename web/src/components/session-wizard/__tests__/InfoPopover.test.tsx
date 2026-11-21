// @vitest-environment jsdom
// Keyboard opening and Tab order need a real browser: tests/wizard-worktree-info.spec.ts.
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { InfoPopover } from "../InfoPopover";

afterEach(cleanup);

function renderPopover() {
  render(
    <>
      <InfoPopover title="What's a worktree?">
        <a href="/settings/worktree">Worktree settings ›</a>
      </InfoPopover>
      <p>outside</p>
    </>,
  );
  return screen.getByRole("button", { name: "What's a worktree?" });
}

const dialog = () => screen.queryByRole("dialog", { name: "What's a worktree?" });

describe("InfoPopover", () => {
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

  it("closes when the page under it scrolls", () => {
    renderPopover();
    fireEvent.click(screen.getByRole("button", { name: "What's a worktree?" }));
    fireEvent.scroll(screen.getByText("outside"));
    expect(dialog()).toBeNull();
  });
});

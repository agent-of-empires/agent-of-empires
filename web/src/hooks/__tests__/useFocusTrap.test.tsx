// @vitest-environment jsdom

import { useRef } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { useFocusTrap } from "../useFocusTrap";

afterEach(cleanup);

function Modal({ active = true, restoreFocus = false }: { active?: boolean; restoreFocus?: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  useFocusTrap(() => ref.current, { active, restoreFocus });
  return (
    <div ref={ref} role="dialog" aria-modal="true">
      <button>first</button>
      <input type="radio" name="pick" aria-label="unchecked" />
      <input type="radio" name="pick" aria-label="checked" defaultChecked />
      <button disabled>disabled</button>
      <div role="dialog" aria-modal="true">
        <button>nested</button>
      </div>
      <button>last</button>
      <input type="radio" name="unpicked" aria-label="unpicked first" />
      <input type="radio" name="unpicked" aria-label="unpicked second" />
    </div>
  );
}

function Page({ open, ...props }: { open: boolean; active?: boolean; restoreFocus?: boolean }) {
  return (
    <>
      <button>outside</button>
      {open && <Modal {...props} />}
    </>
  );
}

const button = (name: string) => screen.getByRole("button", { name });
/** Presses Tab where focus is; true when the trap took the key. */
const tab = (shiftKey = false) => !fireEvent.keyDown(document.activeElement ?? document.body, { key: "Tab", shiftKey });

describe("useFocusTrap", () => {
  it("wraps at either end and leaves Tab between stops to the browser", () => {
    render(<Page open />);
    // A radio group with nothing picked is still one stop, entered at its first radio.
    const lastStop = screen.getByRole("radio", { name: "unpicked first" });
    lastStop.focus();
    expect(tab()).toBe(true);
    expect(document.activeElement).toBe(button("first"));
    expect(tab(true)).toBe(true);
    expect(document.activeElement).toBe(lastStop);
    for (const between of [screen.getByRole("radio", { name: "checked" }), button("last")]) {
      between.focus();
      expect(tab()).toBe(false);
    }
  });

  it("brings Tab from outside the modal back in", () => {
    render(<Page open />);
    button("outside").focus();
    expect(tab()).toBe(true);
    expect(document.activeElement).toBe(button("first"));
    button("outside").focus();
    expect(tab(true)).toBe(true);
    expect(document.activeElement).toBe(screen.getByRole("radio", { name: "unpicked first" }));
  });

  it("leaves the key to a modal opened inside it, and does nothing while inactive", () => {
    render(<Page open />);
    button("nested").focus();
    expect(tab()).toBe(false);
    cleanup();
    render(<Page open active={false} />);
    screen.getByRole("radio", { name: "unpicked first" }).focus();
    expect(tab()).toBe(false);
  });

  it("leaves focus outside both to the modal opened last", () => {
    function Confirm() {
      const ref = useRef<HTMLDivElement>(null);
      useFocusTrap(() => ref.current);
      return (
        <div ref={ref} role="dialog" aria-modal="true">
          <button>confirm</button>
        </div>
      );
    }
    render(
      <>
        <Page open />
        <Confirm />
      </>,
    );
    button("outside").focus();
    expect(tab()).toBe(true);
    expect(document.activeElement).toBe(button("confirm"));
  });

  it("returns focus to the opener on close only when asked", () => {
    for (const restoreFocus of [true, false]) {
      const { rerender } = render(<Page open={false} />);
      button("outside").focus();
      rerender(<Page open restoreFocus={restoreFocus} />);
      button("first").focus();
      rerender(<Page open={false} />);
      expect(document.activeElement === button("outside")).toBe(restoreFocus);
      cleanup();
    }
  });
});

// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MENU_ITEM, MenuPopover } from "../SuggestionStartMenu";

afterEach(cleanup);

function Menu({ items, tick }: { items: string[]; tick: number }) {
  return (
    <MenuPopover
      label="Menu"
      trigger={({ toggle }) => (
        <button type="button" aria-haspopup="menu" onClick={toggle} data-tick={tick}>
          Open
        </button>
      )}
    >
      {() =>
        items.map((i) => (
          <button key={i} type="button" role="menuitem" className={MENU_ITEM}>
            {i}
          </button>
        ))
      }
    </MenuPopover>
  );
}

describe("MenuPopover", () => {
  it("focuses the first item once it arrives, then leaves focus alone on later renders", () => {
    const { rerender } = render(<Menu items={[]} tick={0} />);
    fireEvent.click(screen.getByRole("button", { name: "Open" }));
    rerender(<Menu items={["a", "b"]} tick={0} />);
    expect(document.activeElement).toBe(screen.getByRole("menuitem", { name: "a" }));

    (document.activeElement as HTMLElement).blur();
    rerender(<Menu items={["a", "b"]} tick={1} />);
    expect(document.activeElement).toBe(document.body);
  });
});

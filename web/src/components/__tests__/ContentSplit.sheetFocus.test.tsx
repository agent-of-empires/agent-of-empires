// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { ContentSplit } from "../ContentSplit";

describe("ContentSplit sheet", () => {
  it("keeps Tab and Shift+Tab inside the open sheet and makes the main view inert", () => {
    render(
      <ContentSplit
        left={<button>behind</button>}
        right={
          <>
            <button>first</button>
            <button>last</button>
          </>
        }
        collapsed={false}
        onToggleCollapse={() => {}}
        sheet
      />,
    );
    const [first, last] = [screen.getByText("first"), screen.getByText("last")];
    expect(screen.getByText("behind").closest("[inert]")).not.toBeNull();

    last.focus();
    fireEvent.keyDown(document.activeElement!, { key: "Tab" });
    expect(document.activeElement).toBe(first);

    fireEvent.keyDown(document.activeElement!, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(last);
  });
});

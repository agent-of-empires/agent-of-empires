// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { DiffScopePicker } from "../DiffScopePicker";
import { DiffScopeContext, NO_LAST_TURN, type DiffScopeState } from "../../../lib/diffScope";

function setup(state: Partial<DiffScopeState>) {
  const onChange = vi.fn();
  render(
    <DiffScopeContext.Provider value={{ scope: "branch", requested: "branch", lastTurnAt: null, onChange, ...state }}>
      <DiffScopePicker />
    </DiffScopeContext.Provider>,
  );
  return onChange;
}

describe("DiffScopePicker", () => {
  it("marks the shown scope and picks another", () => {
    const onChange = setup({ scope: "uncommitted", lastTurnAt: "2026-11-25T12:00:00Z" });
    expect(screen.getByRole("radio", { name: "Uncommitted" }).getAttribute("aria-checked")).toBe("true");
    fireEvent.click(screen.getByRole("radio", { name: "Last turn" }));
    expect(onChange).toHaveBeenCalledWith("last_turn");
  });

  it("disables Last turn with the reason until a turn has a snapshot", () => {
    setup({});
    const lastTurn = screen.getByRole("radio", { name: "Last turn" }) as HTMLButtonElement;
    expect([lastTurn.disabled, lastTurn.title]).toEqual([true, NO_LAST_TURN]);
  });

  it("hides where the scope can't change", () => {
    render(<DiffScopePicker />);
    expect(screen.queryByRole("radiogroup")).toBeNull();
  });
});

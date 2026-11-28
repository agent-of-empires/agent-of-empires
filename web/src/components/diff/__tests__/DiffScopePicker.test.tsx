// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { DiffScopePicker } from "../DiffScopePicker";
import { DiffScopeContext, type DiffScopeState } from "../../../lib/diffScope";

function setup(state: Partial<DiffScopeState>) {
  const onChange = vi.fn();
  render(
    <DiffScopeContext.Provider value={{ scope: "branch", onChange, ...state }}>
      <DiffScopePicker />
    </DiffScopeContext.Provider>,
  );
  return onChange;
}

describe("DiffScopePicker", () => {
  it("marks the shown scope and picks another", () => {
    const onChange = setup({ scope: "uncommitted" });
    expect(screen.getByRole("radio", { name: "Uncommitted" }).getAttribute("aria-checked")).toBe("true");
    fireEvent.click(screen.getByRole("radio", { name: "Branch" }));
    expect(onChange).toHaveBeenCalledWith("branch");
  });

  it("hides where the scope can't change", () => {
    render(<DiffScopePicker />);
    expect(screen.queryByRole("radiogroup")).toBeNull();
  });
});

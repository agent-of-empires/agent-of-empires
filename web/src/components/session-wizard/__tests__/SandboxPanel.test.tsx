// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { SandboxPanel } from "../steps/SandboxPanel";
import { buildCreateRequest } from "../createRequest";
import { initialData, type WizardData } from "../wizardReducer";

afterEach(cleanup);

function renderPanel(overrides: Partial<WizardData> = {}) {
  const onChange = vi.fn();
  render(<SandboxPanel data={{ ...initialData, sandboxEnabled: true, ...overrides }} onChange={onChange} />);
  return { onChange };
}

describe("SandboxPanel env lists", () => {
  it("edits the host hook list separately from the container list", () => {
    const { onChange } = renderPanel({ extraEnv: ["A=1"], hookEnv: ["H=2"] });
    expect(screen.getByText("Container environment")).toBeTruthy();
    expect(screen.getByText("Host hook environment")).toBeTruthy();
    fireEvent.change(screen.getByDisplayValue("H=2"), { target: { value: "H=3" } });
    expect(onChange).toHaveBeenCalledWith("hookEnv", ["H=3"]);
    expect(onChange).not.toHaveBeenCalledWith("extraEnv", expect.anything());
  });

  it("names each list's controls so the two lists are distinguishable", () => {
    const { onChange } = renderPanel({ extraEnv: ["A=1"], hookEnv: ["H=2"] });
    expect(screen.getByRole("group", { name: "Host hook environment" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Remove Host hook environment 1" }));
    fireEvent.click(screen.getByRole("button", { name: "Add Container environment" }));
    expect(onChange).toHaveBeenCalledWith("hookEnv", []);
    expect(onChange).toHaveBeenCalledWith("extraEnv", ["A=1", ""]);
  });
});

describe("buildCreateRequest hook_env", () => {
  const build = (over: Partial<WizardData>) => buildCreateRequest({ ...initialData, ...over }, false);

  it("sends non-empty entries only with sandbox on, apart from extra_env", () => {
    const req = build({ sandboxEnabled: true, extraEnv: ["A=1"], hookEnv: ["H=2", ""] });
    expect(req.hook_env).toEqual(["H=2"]);
    expect(req.extra_env).toEqual(["A=1"]);
  });

  it("omits it without sandbox or with only blank rows", () => {
    expect(build({ sandboxEnabled: false, hookEnv: ["H=2"] }).hook_env).toBeUndefined();
    expect(build({ sandboxEnabled: true, hookEnv: [""] }).hook_env).toBeUndefined();
  });
});

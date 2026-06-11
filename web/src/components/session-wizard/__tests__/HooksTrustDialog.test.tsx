// @vitest-environment jsdom
//
// Vitest coverage for the on_create hooks-trust confirm dialog (#2066). The
// mocked Playwright spec exercises the wizard wiring end to end; the
// component's own render + handlers (command list, optional MCP note,
// Trust/Cancel, overlay + keyboard handling, and the confirm catch branch)
// are covered directly here.
//
// Note: this project does not register @testing-library/jest-dom, so
// assertions use plain DOM properties (textContent, getAttribute, disabled).

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { HooksTrustDialog } from "../HooksTrustDialog";

afterEach(() => {
  cleanup();
});

function setup(
  overrides: {
    onCreate?: string[];
    onLaunch?: string[];
    onDestroy?: string[];
    needsMcpTrust?: boolean;
    onConfirm?: () => Promise<void> | void;
  } = {},
) {
  const onConfirm = overrides.onConfirm ?? vi.fn();
  const onCancel = vi.fn();
  const utils = render(
    <HooksTrustDialog
      onCreate={overrides.onCreate ?? ["bash scripts/setup-worktree.sh", "cp .env.example .env"]}
      onLaunch={overrides.onLaunch ?? []}
      onDestroy={overrides.onDestroy ?? []}
      needsMcpTrust={overrides.needsMcpTrust ?? false}
      onConfirm={onConfirm}
      onCancel={onCancel}
    />,
  );
  return { onConfirm, onCancel, ...utils };
}

describe("HooksTrustDialog (#2066)", () => {
  it("lists each on_create command", () => {
    const { getByTestId } = setup();
    const list = getByTestId("hooks-trust-list");
    expect(list.textContent).toContain("bash scripts/setup-worktree.sh");
    expect(list.textContent).toContain("cp .env.example .env");
  });

  it("lists on_launch and on_destroy groups only when non-empty", () => {
    const { getByTestId } = setup({ onLaunch: ["npm start"], onDestroy: ["rm /tmp/seed"] });
    const list = getByTestId("hooks-trust-list");
    expect(list.textContent).toContain("on_launch");
    expect(list.textContent).toContain("npm start");
    expect(list.textContent).toContain("on_destroy");
    expect(list.textContent).toContain("rm /tmp/seed");
  });

  it("omits empty hook groups", () => {
    const { getByTestId } = setup();
    const list = getByTestId("hooks-trust-list");
    expect(list.textContent).not.toContain("on_launch");
    expect(list.textContent).not.toContain("on_destroy");
  });

  it("mentions .mcp.json only when it also needs trust", () => {
    const { getByTestId } = setup({ needsMcpTrust: true });
    expect(getByTestId("hooks-trust-dialog").textContent).toContain(".mcp.json");
  });

  it("omits the .mcp.json note when MCP trust is not needed", () => {
    const { getByTestId } = setup({ needsMcpTrust: false });
    expect(getByTestId("hooks-trust-dialog").textContent).not.toContain(".mcp.json");
  });

  it("Proceed invokes onConfirm", () => {
    const { getByTestId, onConfirm } = setup();
    fireEvent.click(getByTestId("hooks-trust-proceed"));
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("Cancel and the overlay backdrop both cancel; an inner click does not", () => {
    const { getByText, getByTestId, onCancel } = setup();
    fireEvent.click(getByText("Cancel"));
    expect(onCancel).toHaveBeenCalledTimes(1);
    fireEvent.click(getByTestId("hooks-trust-dialog"));
    expect(onCancel).toHaveBeenCalledTimes(2);
    fireEvent.click(getByTestId("hooks-trust-list"));
    expect(onCancel).toHaveBeenCalledTimes(2);
  });

  it("Escape cancels and Enter confirms from the document body", () => {
    const { onConfirm, onCancel } = setup();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(document.body, { key: "Enter" });
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("re-enables Proceed when onConfirm rejects", async () => {
    const onConfirm = vi.fn().mockRejectedValue(new Error("create failed"));
    const { getByTestId } = setup({ onConfirm });
    const proceed = getByTestId("hooks-trust-proceed") as HTMLButtonElement;
    fireEvent.click(proceed);
    await waitFor(() => expect(proceed.disabled).toBe(false));
  });
});

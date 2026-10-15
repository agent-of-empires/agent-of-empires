// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { HooksTrustDialog } from "../HooksTrustDialog";
import { VolumeIgnoresGlobDialog } from "../VolumeIgnoresGlobDialog";
import type { VolumeIgnoresGlobPreview } from "../../../lib/api";

afterEach(cleanup);

const TWO_PATTERNS: VolumeIgnoresGlobPreview[] = [
  { pattern: "**/bin", matched_paths: ["/workspace/x/src/bin", "/workspace/x/tests/bin"] },
  { pattern: "**/obj", matched_paths: ["/workspace/x/src/obj"] },
];

function renderHooks(props: Partial<Parameters<typeof HooksTrustDialog>[0]> = {}) {
  const onConfirm = vi.fn();
  const onCancel = vi.fn();
  render(
    <HooksTrustDialog
      onCreate={["bash scripts/setup-worktree.sh", "cp .env.example .env"]}
      onLaunch={[]}
      onDestroy={[]}
      needsMcpTrust={false}
      onConfirm={onConfirm}
      onCancel={onCancel}
      {...props}
    />,
  );
  return { onConfirm, onCancel };
}

function renderGlobs(props: Partial<Parameters<typeof VolumeIgnoresGlobDialog>[0]> = {}) {
  const onConfirm = vi.fn();
  const onCancel = vi.fn();
  render(<VolumeIgnoresGlobDialog globs={TWO_PATTERNS} onConfirm={onConfirm} onCancel={onCancel} {...props} />);
  return { onConfirm, onCancel };
}

describe.each([
  ["hooks-trust", renderHooks, "hooks-trust-list"],
  ["volume-ignores-glob", renderGlobs, "volume-ignores-glob-list"],
] as const)("%s dialog shell", (prefix, setup, innerTestId) => {
  it("Proceed and Enter confirm; Cancel, the backdrop, and Escape cancel, an inner click does not", () => {
    const { onConfirm, onCancel } = setup();
    fireEvent.click(screen.getByText("Cancel"));
    fireEvent.click(screen.getByTestId(`${prefix}-dialog`));
    fireEvent.click(screen.getByTestId(innerTestId));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledTimes(3);
    fireEvent.click(screen.getByTestId(`${prefix}-proceed`));
    expect(onConfirm).toHaveBeenCalledTimes(1);
    cleanup();
    const byEnter = setup();
    fireEvent.keyDown(document.body, { key: "Enter" });
    expect(byEnter.onConfirm).toHaveBeenCalledTimes(1);
  });

  it("re-enables Proceed when onConfirm rejects", async () => {
    setup({ onConfirm: vi.fn().mockRejectedValue(new Error("create failed")) });
    const proceed = screen.getByTestId(`${prefix}-proceed`) as HTMLButtonElement;
    fireEvent.click(proceed);
    await waitFor(() => expect(proceed.disabled).toBe(false));
  });
});

describe("HooksTrustDialog", () => {
  it("lists only non-empty hook groups", () => {
    renderHooks({ onLaunch: ["npm start"] });
    const list = screen.getByTestId("hooks-trust-list").textContent;
    for (const text of ["bash scripts/setup-worktree.sh", "cp .env.example .env", "on_launch", "npm start"]) {
      expect(list).toContain(text);
    }
    expect(list).not.toContain("on_destroy");
  });

  it.each([true, false])("mentions .mcp.json only when it needs trust (%s)", (needsMcpTrust) => {
    renderHooks({ needsMcpTrust });
    expect(screen.getByTestId("hooks-trust-dialog").textContent?.includes(".mcp.json")).toBe(needsMcpTrust);
  });
});

describe("VolumeIgnoresGlobDialog", () => {
  it.each([false, true])("confirms with dontShowAgain=%s", (tick) => {
    const { onConfirm } = renderGlobs();
    const checkbox = screen.getByTestId("volume-ignores-glob-dont-show-again");
    if (tick) fireEvent.click(checkbox);
    expect(checkbox.getAttribute("data-checked")).toBe(String(tick));
    fireEvent.click(screen.getByTestId("volume-ignores-glob-proceed"));
    expect(onConfirm).toHaveBeenCalledWith(tick);
  });
});

// @vitest-environment jsdom
//
// Vitest coverage for the glob volume_ignores confirm dialog (#2045). The
// mocked Playwright spec exercises the wizard wiring end to end, but the
// component's own render + handlers (pattern list, match-count pluralization,
// checkbox, Proceed/Cancel, overlay + keyboard handling, and the confirm
// catch branch) are covered directly here.
//
// Note: this project does not register @testing-library/jest-dom, so
// assertions use plain DOM properties (textContent, getAttribute, disabled).

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { VolumeIgnoresGlobDialog } from "../VolumeIgnoresGlobDialog";
import type { VolumeIgnoresGlobPreview } from "../../../lib/api";

afterEach(() => {
  cleanup();
});

const TWO_PATTERNS: VolumeIgnoresGlobPreview[] = [
  { pattern: "**/bin", matched_paths: ["/workspace/x/src/bin", "/workspace/x/tests/bin"] },
  { pattern: "**/obj", matched_paths: ["/workspace/x/src/obj"] },
];

function setup(
  overrides: { globs?: VolumeIgnoresGlobPreview[]; onConfirm?: (d: boolean) => Promise<void> | void } = {},
) {
  const onConfirm = overrides.onConfirm ?? vi.fn();
  const onCancel = vi.fn();
  const utils = render(
    <VolumeIgnoresGlobDialog globs={overrides.globs ?? TWO_PATTERNS} onConfirm={onConfirm} onCancel={onCancel} />,
  );
  return { onConfirm, onCancel, ...utils };
}

describe("VolumeIgnoresGlobDialog (#2045)", () => {
  it("lists each pattern with its match count and the plural total", () => {
    const { getByTestId } = setup();
    const list = getByTestId("volume-ignores-glob-list");
    expect(list.textContent).toContain("**/bin");
    expect(list.textContent).toContain("**/obj");
    // Three matched dirs total -> plural "directories".
    const dialog = getByTestId("volume-ignores-glob-dialog");
    expect(dialog.textContent).toContain("3");
    expect(dialog.textContent).toContain("directories");
  });

  it("uses the singular when exactly one directory matches", () => {
    const { getByTestId } = setup({
      globs: [{ pattern: "**/bin", matched_paths: ["/workspace/x/bin"] }],
    });
    const dialog = getByTestId("volume-ignores-glob-dialog");
    expect(dialog.textContent).toContain("director");
    expect(dialog.textContent).not.toContain("directories");
  });

  it("Proceed without the checkbox confirms with dontShowAgain=false", () => {
    const { getByTestId, onConfirm } = setup();
    fireEvent.click(getByTestId("volume-ignores-glob-proceed"));
    expect(onConfirm).toHaveBeenCalledWith(false);
  });

  it("toggling the checkbox then Proceed confirms with dontShowAgain=true", () => {
    const { getByTestId, onConfirm } = setup();
    const checkbox = getByTestId("volume-ignores-glob-dont-show-again");
    expect(checkbox.getAttribute("data-checked")).toBe("false");
    fireEvent.click(checkbox);
    expect(checkbox.getAttribute("data-checked")).toBe("true");
    fireEvent.click(getByTestId("volume-ignores-glob-proceed"));
    expect(onConfirm).toHaveBeenCalledWith(true);
  });

  it("Cancel and the overlay backdrop both cancel; an inner click does not", () => {
    const { getByText, getByTestId, onCancel } = setup();
    fireEvent.click(getByText("Cancel"));
    expect(onCancel).toHaveBeenCalledTimes(1);
    // Backdrop click cancels; clicking inside the panel is stopped.
    fireEvent.click(getByTestId("volume-ignores-glob-dialog"));
    expect(onCancel).toHaveBeenCalledTimes(2);
    fireEvent.click(getByTestId("volume-ignores-glob-list"));
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
    const proceed = getByTestId("volume-ignores-glob-proceed") as HTMLButtonElement;
    fireEvent.click(proceed);
    // The catch branch clears the in-flight state so the user can retry.
    await waitFor(() => expect(proceed.disabled).toBe(false));
  });
});

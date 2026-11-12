// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { CreateFailureBox } from "../CreateFailureBox";
import { failureView } from "../createFailureView";
import type { CreateFailure } from "../../../lib/types";

vi.mock("../../../lib/clipboard", () => ({ writeClipboard: vi.fn().mockResolvedValue(true) }));
import { writeClipboard } from "../../../lib/clipboard";

afterEach(cleanup);

const REF = "3f9c1a2b-77d0-4e0b-9b1f-0d3b5c2a11a1";
const output = Array.from({ length: 12 }, (_, i) => `line ${i + 1}`);

function mount(failure: CreateFailure) {
  const onAction = vi.fn();
  const view = failureView(failure, { sandboxEnabled: true, runtimeDown: false });
  render(<CreateFailureBox failure={failure} view={view} output={output} onAction={onAction} />);
  return { onAction, box: screen.getByRole("alert") };
}

it("names the step, offers its actions and copies the full reference", async () => {
  const { onAction, box } = mount({
    code: "sandbox_unavailable",
    stage: "starting_container",
    message: "Podman isn't running, so the Sandbox can't start.",
    reference: REF,
  });
  expect(document.activeElement).toBe(box);
  expect(box.textContent).toContain("It failed while starting the container.");
  expect(box.textContent).toContain("Podman isn't running");
  expect(screen.getByTestId("create-failure-reference").textContent).toBe("Reference 3f9c1a2b… · in debug.log");
  expect(box.textContent).not.toMatch(/X-Request-Id/i);
  expect(screen.queryByTestId("create-failure-output")).toBeNull();

  fireEvent.click(screen.getByTestId("create-failure-launch_without_sandbox"));
  expect(onAction).toHaveBeenCalledWith("launch_without_sandbox");
  fireEvent.click(screen.getByRole("button", { name: "Copy" }));
  expect(writeClipboard).toHaveBeenCalledWith(REF);
  await waitFor(() => expect(screen.getByRole("button", { name: "Copied" })).toBeTruthy());
});

it("shows a failed hook's last output lines and no reference line without one", () => {
  mount({
    code: "hook_failed",
    stage: "running_hooks",
    message: "An on_create hook exited with code 2.",
    reference: null,
  });
  expect(screen.getByTestId("create-failure-output").textContent).toBe(output.slice(-8).join("\n"));
  expect(screen.queryByTestId("create-failure-reference")).toBeNull();
});

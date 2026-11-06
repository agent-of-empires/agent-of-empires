// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { expect, it, vi } from "vitest";
import type { SetupStep } from "../../../lib/api";
import { SetupBanner } from "../SetupBanner";

const step = (key: SetupStep["key"], over: Partial<SetupStep> = {}): SetupStep => ({
  key,
  required: false,
  configured: false,
  confirmed: false,
  skipped: false,
  ...over,
});

it("counts configured steps, links each open one to its settings, and dismisses", () => {
  const steps = [
    step("theme_telemetry", { configured: true }),
    step("agents", { configured: true, required: true }),
    step("project", { configured: true }),
    step("worktrees", { configured: true }),
    step("sandbox", { skipped: true }),
    step("notifications"),
  ];
  const onDismiss = vi.fn();
  render(
    <MemoryRouter>
      <SetupBanner steps={steps} open={steps.slice(4)} onDismiss={onDismiss} />
    </MemoryRouter>,
  );
  expect(screen.getByText("4 of 6 done. Nothing here blocks you.")).toBeTruthy();
  expect(screen.getByRole("link", { name: /Resume setup/ }).getAttribute("href")).toBe("/setup");
  expect(screen.getByText("Skipped")).toBeTruthy();
  expect(screen.getByRole("link", { name: "Settings › Sandbox" }).getAttribute("href")).toBe("/settings/sandbox");
  expect(screen.getByRole("link", { name: "Settings › Notifications" }).getAttribute("href")).toBe(
    "/settings/notifications",
  );
  fireEvent.click(screen.getByRole("button", { name: "Dismiss setup checklist" }));
  expect(onDismiss).toHaveBeenCalledOnce();
});

// @vitest-environment jsdom
//
// User story (ported from the live Playwright wizard spec): the
// new-session wizard shows the exact resolved launch command
// (post-override, post-arg-resolution) in the review step, and lets
// the user edit the command inline without duplicating the registry
// args that the structured view always appends. Closes #1911.
//
// The resolver matrix itself is covered by lib/launchCommand.test.ts;
// this file covers the ReviewStep wiring: the launch-command row shows
// prefix + read-only suffix, and an inline edit writes only the prefix
// back to the per-session command override.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { ReviewStep } from "../steps/ReviewStep";
import type { AgentInfo } from "../../../lib/types";
import type { StepDef } from "../StepIndicator";

const AGENTS: AgentInfo[] = [
  {
    name: "opencode",
    kind: "builtin",
    binary: "opencode",
    host_only: false,
    installed: true,
    install_hint: "",
    acp_capable: true,
    acp_command: "opencode",
    acp_args: ["acp"],
  },
  {
    name: "claude",
    kind: "builtin",
    binary: "claude",
    host_only: false,
    installed: true,
    install_hint: "",
    acp_capable: true,
    acp_command: "claude-agent-acp",
  },
];

const STEPS: StepDef[] = [
  { id: "project", label: "Project" },
  { id: "session", label: "Session" },
  { id: "agent", label: "Agent" },
  { id: "review", label: "Review" },
];

function baseData(overrides: Record<string, unknown> = {}) {
  return {
    path: "/tmp/project",
    title: "story",
    worktreeBranch: "",
    useWorktree: false,
    attachExisting: false,
    baseBranch: "",
    group: "",
    tool: "opencode",
    profile: "",
    profileDirty: false,
    yoloMode: false,
    sandboxEnabled: false,
    sandboxImage: "",
    extraArgs: "",
    customInstruction: "",
    commandOverride: "",
    scratch: false,
    useStructuredView: true,
    ...overrides,
  };
}

function renderReview(data: ReturnType<typeof baseData>, onChange = vi.fn()) {
  const utils = render(
    <ReviewStep
      data={data}
      onChange={onChange}
      agents={AGENTS}
      isSubmitting={false}
      error={null}
      onSubmit={() => {}}
      onJumpTo={() => {}}
      steps={STEPS}
    />,
  );
  return { ...utils, onChange };
}

afterEach(() => {
  cleanup();
});

describe("ReviewStep resolved launch command (#1911)", () => {
  it("shows the registry command plus args, not the bare binary", () => {
    const { getByTestId } = renderReview(baseData());
    expect(getByTestId("launch-command-row").textContent).toContain("opencode acp");
  });

  it("prefers the structured view launcher over the binary (claude -> claude-agent-acp)", () => {
    const { getByTestId } = renderReview(baseData({ tool: "claude" }));
    expect(getByTestId("launch-command-row").textContent).toContain("claude-agent-acp");
  });

  it("editing the command writes only the prefix override; registry args survive exactly once", () => {
    const onChange = vi.fn();
    const first = renderReview(baseData(), onChange);

    fireEvent.click(first.getByTestId("launch-command-row"));
    const input = first.getByTestId("launch-command-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "opencode-plannotator" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(onChange).toHaveBeenCalledWith("commandOverride", "opencode-plannotator");
    first.unmount();

    // The wizard re-renders the step with the stored override; the row
    // must show the override + the registry arg exactly once (no
    // "opencode-plannotator acp acp").
    const second = renderReview(baseData({ commandOverride: "opencode-plannotator" }));
    const text = second.getByTestId("launch-command-row").textContent ?? "";
    expect(text).toContain("opencode-plannotator acp");
    expect(text.match(/acp/g)).toHaveLength(1);
  });

  it("pasting the full command including the suffix stores a prefix-only override", () => {
    const onChange = vi.fn();
    const { getByTestId } = renderReview(baseData(), onChange);

    fireEvent.click(getByTestId("launch-command-row"));
    const input = getByTestId("launch-command-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "opencode-plannotator acp" } });
    fireEvent.keyDown(input, { key: "Enter" });

    // The trailing registry arg is stripped before storing so the
    // backend does not re-append it on launch.
    expect(onChange).toHaveBeenCalledWith("commandOverride", "opencode-plannotator");
  });
});

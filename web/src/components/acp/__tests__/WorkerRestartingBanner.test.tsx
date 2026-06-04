// @vitest-environment jsdom
//
// Banner copy contract for the three reasons that land in the
// WorkerRestartingBanner: restart_pending (generic restart),
// agent_unresponsive (cancel-escalation watchdog, #1196), and
// prompt_orphaned (silent-orphan watchdog, #1240). The copy is the
// only thing the user sees that distinguishes the failure modes, so
// regressing it would silently lump the orphan path back under the
// agent_unresponsive banner.

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";

import { WorkerRestartingBanner } from "../StructuredView";

describe("WorkerRestartingBanner (#1240)", () => {
  it("renders generic restart copy when neither flag is set", () => {
    const { container } = render(
      <WorkerRestartingBanner
        agentUnresponsive={false}
        agentOrphaned={false}
      />,
    );
    expect(container.textContent).toContain("Restarting structured view worker");
    expect(container.textContent).not.toContain("stopped responding to cancel");
    expect(container.textContent).not.toContain(
      "finished but didn't notify the daemon",
    );
  });

  it("renders cancel-escalation copy when agentUnresponsive is true", () => {
    const { container } = render(
      <WorkerRestartingBanner agentUnresponsive={true} agentOrphaned={false} />,
    );
    expect(container.textContent).toContain(
      "Agent stopped responding to cancel",
    );
  });

  it("renders silent-orphan copy when agentOrphaned is true", () => {
    const { container } = render(
      <WorkerRestartingBanner agentUnresponsive={false} agentOrphaned={true} />,
    );
    expect(container.textContent).toContain(
      "Agent finished but didn't notify the daemon",
    );
  });

  it("prefers agentOrphaned copy when both flags are true", () => {
    // The supervisor never publishes both reasons for the same turn,
    // but the reducer can briefly land in the both-true state during
    // a cancel-escalation race. The banner must pick a deterministic
    // copy in that window; agentOrphaned (more specific failure) wins.
    const { container } = render(
      <WorkerRestartingBanner agentUnresponsive={true} agentOrphaned={true} />,
    );
    expect(container.textContent).toContain(
      "Agent finished but didn't notify the daemon",
    );
    expect(container.textContent).not.toContain(
      "Agent stopped responding to cancel",
    );
  });
});

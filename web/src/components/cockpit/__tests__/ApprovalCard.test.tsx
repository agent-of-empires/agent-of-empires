// @vitest-environment jsdom
//
// #1713: Gemini's confirm-required tools ship no raw_input, so the
// backend sends an empty args_preview (not the literal "null"). The
// approval card must render a dedicated empty-state instead of an empty
// <pre> or the word "null".

import { describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";
import { ApprovalCard } from "../ApprovalCard";
import type { Approval } from "../../../lib/cockpitTypes";

function approvalWith(argsPreview: string): Approval {
  return {
    nonce: "n1",
    tool_call: {
      id: "t1",
      name: "Write file",
      kind: "edit",
      args_preview: argsPreview,
      started_at: "2026-01-01T00:00:00Z",
    },
    destructive: false,
    requested_at: "2026-01-01T00:00:00Z",
    resolved: null,
  };
}

describe("ApprovalCard args rendering (#1713)", () => {
  it("renders an empty-state instead of literal null when no args provided", () => {
    const { container } = render(
      <ApprovalCard approval={approvalWith("")} onResolve={vi.fn()} />,
    );
    expect(container.textContent).toContain("No raw args provided by agent.");
  });

  it("still renders provided args as a definition list", () => {
    const { container } = render(
      <ApprovalCard
        approval={approvalWith('{"path":"/tmp/x"}')}
        onResolve={vi.fn()}
      />,
    );
    expect(container.textContent).toContain("/tmp/x");
    expect(container.textContent).not.toContain(
      "No raw args provided by agent.",
    );
  });
});

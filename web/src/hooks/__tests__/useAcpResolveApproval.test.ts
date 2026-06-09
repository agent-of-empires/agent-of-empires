// #1821: an approval card must clear when the resolve POST succeeds (204)
// or the daemon reports the nonce already gone (404), without waiting on
// the ApprovalResolved broadcast (which the seq dedupe can swallow). A
// session-gone 404 stays a real error. These exercise the two pure pieces
// the resolveApproval flow is built from: the response classifier and the
// reducer action that drops the card.

import { describe, expect, it } from "vitest";

import { classifyApprovalResolveResponse, reducer, type Action } from "../useAcpSession";
import { emptyAcpState, type Approval } from "../../lib/acpTypes";

function approval(nonce: string): Approval {
  return {
    nonce,
    tool_call: {
      id: `tc-${nonce}`,
      name: "Bash",
      kind: "execute",
      args_preview: "ls",
      started_at: new Date().toISOString(),
    },
    destructive: false,
    requested_at: new Date().toISOString(),
  };
}

describe("classifyApprovalResolveResponse", () => {
  it("treats a 204 success as resolved", () => {
    expect(classifyApprovalResolveResponse(true, 204, "", "n-1")).toEqual({
      kind: "resolved",
    });
  });

  it("treats a 404 naming this nonce as resolved", () => {
    expect(classifyApprovalResolveResponse(false, 404, "no pending approval with nonce n-1", "n-1")).toEqual({
      kind: "resolved",
    });
  });

  it("treats a 404 naming a different nonce as an error", () => {
    // Guards the #1821 contract: a generic / wrong-nonce 404 must not
    // silently clear the clicked card.
    const out = classifyApprovalResolveResponse(false, 404, "no pending approval with nonce other-99", "n-1");
    expect(out.kind).toBe("error");
  });

  it("treats a session-gone 404 as an error", () => {
    const out = classifyApprovalResolveResponse(false, 404, "session has no running agent", "n-1");
    expect(out.kind).toBe("error");
    expect(out.kind === "error" && out.message).toContain("404");
  });

  it("treats a 500 as an error", () => {
    const out = classifyApprovalResolveResponse(false, 500, "boom", "n-1");
    expect(out.kind).toBe("error");
  });
});

describe("reducer approval_resolved_locally", () => {
  it("removes the matching card and clears any error", () => {
    const state = {
      ...emptyAcpState(),
      lastError: "stale error",
      pendingApprovals: [approval("n-1"), approval("n-2")],
    };
    const action: Action = { kind: "approval_resolved_locally", nonce: "n-1" };
    const next = reducer(state, action);
    expect(next.pendingApprovals.map((a) => a.nonce)).toEqual(["n-2"]);
    expect(next.lastError).toBeNull();
  });

  it("is a no-op for an unknown nonce and keeps the existing error", () => {
    // A duplicate/stale action must not quietly clear an unrelated banner.
    const state = {
      ...emptyAcpState(),
      lastError: "unrelated error",
      pendingApprovals: [approval("n-1")],
    };
    const next = reducer(state, {
      kind: "approval_resolved_locally",
      nonce: "missing",
    });
    expect(next.pendingApprovals.map((a) => a.nonce)).toEqual(["n-1"]);
    expect(next.lastError).toBe("unrelated error");
  });
});

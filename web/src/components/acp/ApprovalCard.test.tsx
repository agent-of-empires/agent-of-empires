// @vitest-environment jsdom
//
// Approval card rendering + decision routing. The card is the only
// UI gate between the agent and a destructive action, so the test
// pins:
//   - destructive vs benign chrome distinguishable to a screen
//     reader (role=alertdialog, AlertTriangle vs Shield, label),
//   - benign branch: single-tap Allow / Always / Deny each route
//     `onResolve` with the matching ApprovalDecision,
//   - destructive branch: only Hold-to-allow + Deny; instant click
//     does NOT resolve until LONG_PRESS_MS elapses,
//   - args_preview rendering: parsed JSON → <dl> with `_aoe_*` keys
//     hidden; non-object → raw <pre>,
//   - offline + rolled-back states disable the action surface.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { ApprovalCard } from "./ApprovalCard";
import type { Approval, ApprovalDecision } from "../../lib/acpTypes";

vi.mock("../../lib/connectionState", () => ({
  useServerDown: () => false,
  OFFLINE_TITLE: "Disconnected",
}));

function makeApproval(over: Partial<Approval> = {}): Approval {
  return {
    nonce: "n-1",
    tool_call: {
      id: "t-1",
      name: "Bash",
      kind: "execute",
      args_preview: JSON.stringify({ command: "ls -al" }),
      started_at: "2026-05-21T00:00:00Z",
    },
    destructive: false,
    requested_at: "2026-05-21T00:00:00Z",
    ...over,
  };
}

afterEach(() => {
  cleanup();
});

describe("ApprovalCard (benign)", () => {
  it("renders the tool name and Approval-needed chrome", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={makeApproval()} onResolve={onResolve} />);
    expect(screen.getByRole("alertdialog", { name: /Approval needed: Bash/i })).toBeTruthy();
    expect(screen.getByText("Approval needed")).toBeTruthy();
    expect(screen.getByText("Bash")).toBeTruthy();
  });

  it("collapses to a command preview and hides args until expanded", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(
      <ApprovalCard
        approval={makeApproval({
          tool_call: {
            id: "t-1",
            name: "Bash",
            kind: "execute",
            args_preview: JSON.stringify({ command: "ls -al", cwd: "/tmp" }),
            started_at: "2026-05-21T00:00:00Z",
          },
        })}
        onResolve={onResolve}
      />,
    );
    // Command preview is in the collapsed header; the args <dl> is not.
    expect(screen.getByText("ls -al")).toBeTruthy();
    expect(screen.queryByText("cwd")).toBeNull();
    expect(screen.queryByText("/tmp")).toBeNull();
    // Action surface stays reachable without expanding.
    expect(screen.getByText("Allow")).toBeTruthy();
    expect(screen.getByText("Deny")).toBeTruthy();
  });

  it("collapses opencode filepath metadata to a path preview", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(
      <ApprovalCard
        approval={makeApproval({
          tool_call: {
            id: "t-1",
            name: "external_directory",
            kind: "other",
            args_preview: JSON.stringify({ filepath: "/tmp/opencode", parentDir: "/tmp" }),
            started_at: "2026-05-21T00:00:00Z",
          },
        })}
        onResolve={onResolve}
      />,
    );
    expect(screen.getByText("/tmp/opencode")).toBeTruthy();
    expect(screen.queryByText("filepath")).toBeNull();
  });

  it("renders the args JSON as a key/value list once expanded", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(
      <ApprovalCard
        approval={makeApproval({
          tool_call: {
            id: "t-1",
            name: "Bash",
            kind: "execute",
            args_preview: JSON.stringify({ command: "ls", cwd: "/tmp" }),
            started_at: "2026-05-21T00:00:00Z",
          },
        })}
        onResolve={onResolve}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /Approval needed/i }));
    expect(screen.getByText("command")).toBeTruthy();
    // "ls" shows in both the header preview and the expanded args row.
    expect(screen.getAllByText("ls")).toHaveLength(2);
    expect(screen.getByText("cwd")).toBeTruthy();
    expect(screen.getByText("/tmp")).toBeTruthy();
  });

  it("toggles the args open and closed on header clicks", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(
      <ApprovalCard
        approval={makeApproval({
          tool_call: {
            id: "t-1",
            name: "Bash",
            kind: "execute",
            args_preview: JSON.stringify({ command: "ls", cwd: "/tmp" }),
            started_at: "2026-05-21T00:00:00Z",
          },
        })}
        onResolve={onResolve}
      />,
    );
    const header = screen.getByRole("button", { name: /Approval needed/i });
    expect(screen.queryByText("cwd")).toBeNull();
    fireEvent.click(header);
    expect(screen.getByText("cwd")).toBeTruthy();
    fireEvent.click(header);
    expect(screen.queryByText("cwd")).toBeNull();
  });

  it("hides bookkeeping keys whose name starts with _aoe_ when expanded", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(
      <ApprovalCard
        approval={makeApproval({
          tool_call: {
            id: "t-1",
            name: "Bash",
            kind: "execute",
            args_preview: JSON.stringify({
              command: "ls",
              _aoe_parent_tool_call_id: "parent-123",
            }),
            started_at: "2026-05-21T00:00:00Z",
          },
        })}
        onResolve={onResolve}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /Approval needed/i }));
    expect(screen.queryByText("_aoe_parent_tool_call_id")).toBeNull();
    expect(screen.queryByText("parent-123")).toBeNull();
    expect(screen.getByText("command")).toBeTruthy();
  });

  it("falls back to a raw pre block when args_preview is not a JSON object", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(
      <ApprovalCard
        approval={makeApproval({
          tool_call: {
            id: "t-1",
            name: "Bash",
            kind: "execute",
            args_preview: "raw text [truncated]",
            started_at: "2026-05-21T00:00:00Z",
          },
        })}
        onResolve={onResolve}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /Approval needed/i }));
    expect(screen.getByText("raw text [truncated]")).toBeTruthy();
  });

  it("offers no expand toggle when there is no args body", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(
      <ApprovalCard
        approval={makeApproval({
          tool_call: {
            id: "t-1",
            name: "Bash",
            kind: "execute",
            args_preview: JSON.stringify({ _aoe_title: "noop" }),
            started_at: "2026-05-21T00:00:00Z",
          },
        })}
        onResolve={onResolve}
      />,
    );
    expect(screen.queryByRole("button", { name: /Approval needed/i })).toBeNull();
  });

  it("routes the Allow button to onResolve('Allow')", async () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={makeApproval()} onResolve={onResolve} />);
    fireEvent.click(screen.getByText("Allow"));
    expect(onResolve).toHaveBeenCalledTimes(1);
    expect(onResolve).toHaveBeenCalledWith<ApprovalDecision[]>("Allow");
  });

  it("routes Always to onResolve('AllowAlways')", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={makeApproval()} onResolve={onResolve} />);
    fireEvent.click(screen.getByText("Always"));
    expect(onResolve).toHaveBeenCalledWith("AllowAlways");
  });

  it("routes Deny to onResolve('Deny')", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={makeApproval()} onResolve={onResolve} />);
    fireEvent.click(screen.getByText("Deny"));
    expect(onResolve).toHaveBeenCalledWith("Deny");
  });

  it("shows the rolled-back message when onResolve rejects", async () => {
    const onResolve = vi.fn().mockRejectedValue(new Error("network"));
    render(<ApprovalCard approval={makeApproval()} onResolve={onResolve} />);
    await act(async () => {
      fireEvent.click(screen.getByText("Allow"));
    });
    expect(screen.getByText(/Could not reach the server/i)).toBeTruthy();
  });
});

describe("ApprovalCard (destructive)", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("renders the destructive chrome (AlertTriangle + 'Destructive action' label)", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={makeApproval({ destructive: true })} onResolve={onResolve} />);
    expect(screen.getByText("Destructive action")).toBeTruthy();
    expect(screen.getByText("Hold to allow")).toBeTruthy();
    expect(screen.queryByText("Always")).toBeNull();
  });

  it("defaults to expanded so the full command is in view", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={makeApproval({ destructive: true })} onResolve={onResolve} />);
    // The args <dl> renders without a click in the destructive branch.
    expect(screen.getByText("command")).toBeTruthy();
  });

  it("does not approve on a quick click of Hold to allow", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={makeApproval({ destructive: true })} onResolve={onResolve} />);
    const btn = screen.getByText("Hold to allow");
    fireEvent.mouseDown(btn);
    act(() => {
      vi.advanceTimersByTime(100);
    });
    fireEvent.mouseUp(btn);
    expect(onResolve).not.toHaveBeenCalled();
  });

  it("approves after a sustained 800ms hold", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={makeApproval({ destructive: true })} onResolve={onResolve} />);
    const btn = screen.getByText("Hold to allow");
    fireEvent.mouseDown(btn);
    act(() => {
      vi.advanceTimersByTime(800);
    });
    expect(onResolve).toHaveBeenCalledTimes(1);
    expect(onResolve).toHaveBeenCalledWith("Allow");
  });

  it("routes Deny without requiring a hold even in destructive mode", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={makeApproval({ destructive: true })} onResolve={onResolve} />);
    fireEvent.click(screen.getByText("Deny"));
    expect(onResolve).toHaveBeenCalledWith("Deny");
  });
});

describe("ApprovalCard (permission identifier humanization)", () => {
  function permissionApproval(name: string): Approval {
    return makeApproval({
      tool_call: {
        id: "t-1",
        name,
        kind: "other",
        args_preview: "",
        started_at: "2026-05-21T00:00:00Z",
      },
    });
  }

  it("humanizes a known permission identifier in the title and accessible name", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={permissionApproval("external_directory")} onResolve={onResolve} />);
    expect(screen.getByText("External directory access")).toBeTruthy();
    expect(screen.getByRole("alertdialog", { name: /Approval needed: External directory access/i })).toBeTruthy();
    // The raw protocol identifier is no longer shown to the user.
    expect(screen.queryByText("external_directory")).toBeNull();
  });

  it("passes an unknown identifier through verbatim", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<ApprovalCard approval={permissionApproval("some_future_kind")} onResolve={onResolve} />);
    expect(screen.getByText("some_future_kind")).toBeTruthy();
  });
});

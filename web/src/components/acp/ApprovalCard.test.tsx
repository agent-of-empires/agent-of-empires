// @vitest-environment jsdom
// The card is the only UI gate before a destructive tool runs, so hold semantics are pinned closely.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { ApprovalCard } from "./ApprovalCard";
import type { Approval, ApprovalOption } from "../../lib/acpTypes";

vi.mock("../../lib/connectionState", () => ({
  useServerDown: () => false,
  OFFLINE_TITLE: "Disconnected",
}));

function makeApproval(over: Partial<Approval> = {}, args: unknown = { command: "ls -al" }, name = "Bash"): Approval {
  return {
    nonce: "n-1",
    tool_call: {
      id: "t-1",
      name,
      kind: "execute",
      args_preview: typeof args === "string" ? args : JSON.stringify(args),
      started_at: "2026-05-21T00:00:00Z",
    },
    destructive: false,
    requested_at: "2026-05-21T00:00:00Z",
    ...over,
  };
}

function mount(approval: Approval, onResolve = vi.fn().mockResolvedValue(undefined)) {
  render(<ApprovalCard approval={approval} onResolve={onResolve} />);
  return onResolve;
}

const header = () => screen.getByRole("button", { name: /Approval needed/i });
const hold = (el: HTMLElement, ms: number, start: "mouseDown" | "touchStart" = "mouseDown") => {
  fireEvent[start](el);
  act(() => {
    vi.advanceTimersByTime(ms);
  });
};

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("ApprovalCard args", () => {
  it("renders the chrome and a collapsed command preview with actions reachable", () => {
    mount(makeApproval({}, { command: "ls -al", cwd: "/tmp" }));
    expect(screen.getByRole("alertdialog", { name: /Approval needed: Bash/i })).toBeTruthy();
    expect(screen.getByText("Approval needed")).toBeTruthy();
    expect(screen.getByText("ls -al")).toBeTruthy();
    expect(screen.queryByText("cwd")).toBeNull();
    expect(screen.getByText("Allow")).toBeTruthy();
    expect(screen.getByText("Deny")).toBeTruthy();
  });

  it("toggles a key/value list without _aoe_ bookkeeping keys", () => {
    mount(makeApproval({}, { command: "ls", cwd: "/tmp", _aoe_parent_tool_call_id: "parent-123" }));
    fireEvent.click(header());
    expect(screen.getByText("command")).toBeTruthy();
    expect(screen.getAllByText("ls")).toHaveLength(2);
    expect(screen.getByText("/tmp")).toBeTruthy();
    expect(screen.queryByText("_aoe_parent_tool_call_id")).toBeNull();
    expect(screen.queryByText("parent-123")).toBeNull();
    fireEvent.click(header());
    expect(screen.queryByText("cwd")).toBeNull();
  });

  it("falls back to a raw pre block for a non-object preview", () => {
    mount(makeApproval({}, "raw text [truncated]"));
    fireEvent.click(header());
    expect(screen.getByText("raw text [truncated]")).toBeTruthy();
  });

  it("offers no toggle when there is no args body", () => {
    mount(makeApproval({}, { _aoe_title: "noop" }));
    expect(screen.queryByRole("button", { name: /Approval needed/i })).toBeNull();
  });

  // Gemini confirm-required tools send no raw input at all.
  it("shows an empty-args state instead of an empty block", () => {
    mount(makeApproval({}, ""));
    expect(screen.getByText("No raw args provided by agent.")).toBeTruthy();
  });

  it("humanizes a known permission identifier and collapses opencode filepath metadata", () => {
    mount(makeApproval({}, { filepath: "/tmp/opencode", parentDir: "/tmp" }, "external_directory"));
    expect(screen.getByRole("alertdialog", { name: "Approval needed: External directory access" })).toBeTruthy();
    expect(screen.queryByText("external_directory")).toBeNull();
    expect(screen.getByText("/tmp/opencode")).toBeTruthy();
    expect(screen.queryByText("filepath")).toBeNull();
  });
});

describe("ApprovalCard decisions", () => {
  it.each([
    ["Allow", "Allow"],
    ["Always", "AllowAlways"],
    ["Deny", "Deny"],
  ])("benign %s resolves %s on a single tap", (button, decision) => {
    const onResolve = mount(makeApproval());
    fireEvent.click(screen.getByText(button));
    expect(onResolve).toHaveBeenCalledTimes(1);
    expect(onResolve).toHaveBeenCalledWith(decision, undefined);
  });

  it("shows the rolled-back message when onResolve rejects", async () => {
    mount(makeApproval(), vi.fn().mockRejectedValue(new Error("network")));
    await act(async () => {
      fireEvent.click(screen.getByText("Allow"));
    });
    expect(screen.getByText(/Could not reach the server/i)).toBeTruthy();
  });

  describe("destructive", () => {
    beforeEach(() => {
      vi.useFakeTimers();
    });

    it("expands by default, drops Always, and only allows after a full hold", () => {
      const onResolve = mount(makeApproval({ destructive: true }));
      expect(screen.getByText("Destructive action")).toBeTruthy();
      expect(screen.getByText("command")).toBeTruthy();
      expect(screen.queryByText("Always")).toBeNull();
      const button = screen.getByText("Hold to allow");
      hold(button, 100);
      fireEvent.mouseUp(button);
      expect(onResolve).not.toHaveBeenCalled();
      hold(button, 800);
      expect(onResolve).toHaveBeenCalledExactlyOnceWith("Allow", undefined);
    });

    it("denies without a hold", () => {
      const onResolve = mount(makeApproval({ destructive: true }));
      fireEvent.click(screen.getByText("Deny"));
      expect(onResolve).toHaveBeenCalledWith("Deny", undefined);
    });
  });
});

describe("ApprovalCard option lists", () => {
  const options = (names: string[], kind: ApprovalOption["kind"], prefix: string) =>
    names.map((name, i) => ({ option_id: `${prefix}-${i}`, name, kind }));
  const question = (over: Partial<Approval> = {}) =>
    makeApproval(
      {
        choice: true,
        options: options(["Option Alpha", "Option Bravo", "Option Charlie"], "allow_once", "choice"),
        ...over,
      },
      { message: "Which plan?" },
      "Pi select",
    );

  it("renders the agent's labels and question body instead of the trio, and posts the picked id", () => {
    const onResolve = mount(question());
    expect(screen.getByRole("alertdialog", { name: /Question: Pi select/i })).toBeTruthy();
    expect(screen.getByText("Which plan?")).toBeTruthy();
    expect(screen.queryByText("Allow")).toBeNull();
    expect(screen.queryByText("Always")).toBeNull();
    fireEvent.click(screen.getByText("Option Charlie"));
    expect(onResolve).toHaveBeenCalledExactlyOnceWith("Allow", "choice-2");
  });

  // A bare Deny would be mapped to the first reject-kind option and sent as an answer.
  it("dismisses by cancelling, while an explicitly picked reject option still answers", () => {
    const rejectList = () => question({ options: options(["Stop here", "Stop and revert"], "reject_once", "no") });
    const dismissed = mount(rejectList());
    fireEvent.click(screen.getByText("Dismiss"));
    expect(dismissed).toHaveBeenCalledExactlyOnceWith("Cancelled", undefined);
    cleanup();
    const picked = mount(rejectList());
    fireEvent.click(screen.getByText("Stop and revert"));
    expect(picked).toHaveBeenCalledExactlyOnceWith("Allow", "no-1");
  });

  it("falls back to the trio when flagged as a choice with no options", () => {
    mount(question({ options: [] }));
    expect(screen.getByText("Allow")).toBeTruthy();
    expect(screen.getByText("Deny")).toBeTruthy();
  });

  describe("destructive", () => {
    beforeEach(() => {
      vi.useFakeTimers();
    });
    const destructive = () =>
      mount(
        makeApproval(
          {
            destructive: true,
            choice: true,
            options: [
              { option_id: "wipe", name: "Delete everything", kind: "allow_once" },
              { option_id: "logs", name: "Delete only logs", kind: "allow_once" },
            ],
          },
          { command: "rm -rf ./build" },
        ),
      );
    const option = (name: string) => screen.getByRole("button", { name });

    it("ignores taps and short holds, and answers with the held option after a full hold", () => {
      const onResolve = destructive();
      const logs = option("Delete only logs");
      fireEvent.click(logs);
      hold(logs, 400);
      fireEvent.mouseUp(logs);
      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(onResolve).not.toHaveBeenCalled();
      hold(logs, 800);
      expect(onResolve).toHaveBeenCalledExactlyOnceWith("Allow", "logs");
    });

    // An orphaned timer would run the answer the user moved away from.
    it("cancels an abandoned hold when a second one starts, and a released hold submits nothing", () => {
      const onResolve = destructive();
      const wipe = option("Delete everything");
      hold(wipe, 700, "touchStart");
      fireEvent.touchCancel(wipe);
      act(() => {
        vi.advanceTimersByTime(2000);
      });
      expect(onResolve).not.toHaveBeenCalled();

      hold(wipe, 400, "touchStart");
      hold(option("Delete only logs"), 400, "touchStart");
      expect(onResolve).not.toHaveBeenCalled();
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(onResolve).toHaveBeenCalledExactlyOnceWith("Allow", "logs");
    });

    it("keeps Dismiss a single click", () => {
      const onResolve = destructive();
      fireEvent.click(screen.getByText("Dismiss"));
      expect(onResolve).toHaveBeenCalledWith("Cancelled", undefined);
    });
  });
});

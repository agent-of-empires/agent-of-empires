// @vitest-environment jsdom
// The card is the only UI gate before a destructive tool runs, so hold semantics are pinned closely.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";

import { AcpFileRefContext } from "./AcpFileRefContext";
import { ApprovalCard } from "./ApprovalCard";
import type { Approval, ApprovalOption } from "../../lib/acpTypes";

vi.mock("../../lib/connectionState", () => ({
  useServerDown: () => false,
  OFFLINE_TITLE: "Disconnected",
}));

// The primitive needs a message context; the plan modal's own wiring is under test.
vi.mock("./Markdown", async () => {
  const { useAcpFileRef } = await import("./AcpFileRefContext");
  return {
    // A "LINK" in the text stands in for an in-repo file link in the rendered plan.
    Markdown: ({ text }: { text: string }) => {
      const { onOpenFileRef } = useAcpFileRef();
      return (
        <div data-testid="plan-rendered">
          {text}
          {text.includes("LINK") && (
            <a href="#plan-file" onClick={() => onOpenFileRef?.({ path: "/repo/PLAN.md" })}>
              plan file
            </a>
          )}
        </div>
      );
    },
  };
});

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

describe("ApprovalCard plan viewer", () => {
  const PLAN = "# Plan\n\nProse with no bullets";
  const withPlan = () => makeApproval({ plan: PLAN }, "{ truncated json", "Ready to code?");

  it("offers the plan only when the approval carries one, even if the args preview is unparsable", () => {
    mount(makeApproval());
    expect(screen.queryByText("View plan")).toBeNull();
    cleanup();
    mount(withPlan());
    expect(screen.getByText("View plan")).toBeTruthy();
  });

  it("switches between the rendered and raw plan, and closing resolves nothing", () => {
    const onResolve = mount(withPlan());
    fireEvent.click(screen.getByText("View plan"));
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByTestId("plan-rendered").textContent).toBe(PLAN);

    fireEvent.click(within(dialog).getByRole("tab", { name: "Raw" }));
    expect(within(dialog).queryByTestId("plan-rendered")).toBeNull();
    expect(within(dialog).getByText(/Prose with no bullets/).tagName).toBe("PRE");

    fireEvent.click(within(dialog).getByText("Close"));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onResolve).not.toHaveBeenCalled();
  });

  it("resolves from the modal footer with the card's decisions", () => {
    for (const [label, decision] of [
      ["Allow", "Allow"],
      ["Always", "AllowAlways"],
      ["Deny", "Deny"],
    ] as const) {
      const onResolve = mount(withPlan());
      fireEvent.click(screen.getByText("View plan"));
      fireEvent.click(within(screen.getByRole("dialog")).getByText(label));
      expect(onResolve).toHaveBeenCalledExactlyOnceWith(decision, undefined);
      cleanup();
    }
  });
  it("shows text past the 16 KiB preview cap in both views", () => {
    const tail = "TAIL-MARKER-PAST-THE-CAP";
    const long = `${"line of plan\n".repeat(2000)}${tail}`;
    expect(long.length).toBeGreaterThan(16 * 1024);
    mount(
      makeApproval({ plan: long }, `{"plan":"${"line of plan\\n".repeat(1500)}\u2026[truncated]`, "Ready to code?"),
    );
    fireEvent.click(screen.getByText("View plan"));
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByTestId("plan-rendered").textContent).toContain(tail);
    fireEvent.click(within(dialog).getByRole("tab", { name: "Raw" }));
    expect(within(dialog).getByText(/TAIL-MARKER-PAST-THE-CAP/).tagName).toBe("PRE");
  });

  it("moves focus into the modal, wraps Tab, closes on Escape and restores focus", () => {
    mount(withPlan());
    const opener = screen.getByText("View plan");
    opener.focus();
    fireEvent.click(opener);
    const dialog = screen.getByRole("dialog");
    const buttons = Array.from(dialog.querySelectorAll("button"));
    expect(dialog.contains(document.activeElement)).toBe(true);

    buttons[buttons.length - 1].focus();
    fireEvent.keyDown(document, { key: "Tab" });
    expect(document.activeElement).toBe(buttons[0]);
    fireEvent.keyDown(document, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(buttons[buttons.length - 1]);

    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(opener);
  });
  it("leaves Tab and Escape to a higher overlay or an inert modal but reclaims plain focus", () => {
    mount(withPlan());
    fireEvent.click(screen.getByText("View plan"));
    const overlay = document.createElement("div");
    overlay.setAttribute("aria-modal", "true");
    const other = document.createElement("input");
    overlay.append(other);
    document.body.append(overlay);
    other.focus();
    for (const init of [{ key: "Tab" }, { key: "Escape" }]) {
      const ev = new KeyboardEvent("keydown", { ...init, bubbles: true, cancelable: true });
      document.dispatchEvent(ev);
      expect(ev.defaultPrevented).toBe(false);
      expect(screen.queryByRole("dialog")).not.toBeNull();
    }
    expect(document.activeElement).toBe(other);
    overlay.remove();

    const plain = document.createElement("textarea");
    document.body.append(plain);
    plain.focus();
    const reached = vi.fn();
    plain.addEventListener("keydown", reached);
    const tab = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
    plain.dispatchEvent(tab);
    expect(tab.defaultPrevented).toBe(true);
    expect(reached).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog").contains(document.activeElement)).toBe(true);
    plain.focus();
    fireEvent.keyDown(plain, { key: "Escape" });
    expect(reached).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).toBeNull();
    plain.remove();
    fireEvent.click(screen.getByText("View plan"));

    const dialog = screen.getByRole("dialog");
    const wrapper = document.createElement("div");
    wrapper.setAttribute("inert", "");
    dialog.parentElement?.insertBefore(wrapper, dialog);
    wrapper.append(dialog);
    (document.activeElement as HTMLElement | null)?.blur();
    for (const init of [{ key: "Tab" }, { key: "Escape" }]) {
      const ev = new KeyboardEvent("keydown", { ...init, bubbles: true, cancelable: true });
      document.dispatchEvent(ev);
      expect(ev.defaultPrevented).toBe(false);
    }
    expect(screen.queryByRole("dialog", { hidden: true })).not.toBeNull();
  });
  it("binds each plan modal's trap to its own dialog when a covered session keeps one open", () => {
    const covered = makeApproval({ plan: PLAN, nonce: "n-a" }, "{}", "Ready to code?");
    const active = makeApproval({ plan: PLAN, nonce: "n-b" }, "{}", "Ready to code?");
    const { container } = render(
      <>
        <div data-testid="covered" inert>
          <ApprovalCard approval={covered} onResolve={vi.fn()} />
        </div>
        <div data-testid="active">
          <ApprovalCard approval={active} onResolve={vi.fn()} />
        </div>
      </>,
    );
    for (const id of ["covered", "active"]) {
      fireEvent.click(within(within(container).getByTestId(id)).getByText("View plan"));
    }
    expect(screen.getAllByRole("dialog", { hidden: true })).toHaveLength(2);

    fireEvent.keyDown(document, { key: "Escape" });
    const left = screen.getAllByRole("dialog", { hidden: true });
    expect(left).toHaveLength(1);
    expect(
      within(container)
        .getByTestId("covered")
        .contains(left[0] ?? null),
    ).toBe(true);
  });
  it("moves between the view tabs with the arrow keys and labels the panel", () => {
    mount(withPlan());
    fireEvent.click(screen.getByText("View plan"));
    const dialog = screen.getByRole("dialog");
    const rendered = within(dialog).getByRole("tab", { name: "Rendered" });
    const raw = within(dialog).getByRole("tab", { name: "Raw" });
    expect(rendered.getAttribute("tabindex")).toBe("0");
    expect(raw.getAttribute("tabindex")).toBe("-1");

    rendered.focus();
    fireEvent.keyDown(rendered, { key: "ArrowRight" });
    expect(raw.getAttribute("aria-selected")).toBe("true");
    expect(document.activeElement).toBe(raw);
    expect(within(dialog).getByRole("tabpanel").getAttribute("aria-labelledby")).toBe(raw.id);

    fireEvent.keyDown(raw, { key: "Home" });
    expect(rendered.getAttribute("aria-selected")).toBe("true");
    expect(document.activeElement).toBe(rendered);
  });

  it("answers a choice approval with its option id from the modal", () => {
    const options: ApprovalOption[] = [
      { option_id: "a", name: "Plan A", kind: "allow_once" },
      { option_id: "b", name: "Plan B", kind: "allow_once" },
    ];
    for (const [label, decision, optionId] of [
      ["Plan B", "Allow", "b"],
      ["Dismiss", "Cancelled", undefined],
    ] as const) {
      const onResolve = mount(makeApproval({ plan: PLAN, choice: true, options }, "{}", "Ready to code?"));
      fireEvent.click(screen.getByText("View plan"));
      const dialog = screen.getByRole("dialog");
      expect(within(dialog).queryByText("Deny")).toBeNull();
      fireEvent.click(within(dialog).getByText(label));
      expect(onResolve).toHaveBeenCalledExactlyOnceWith(decision, optionId);
      cleanup();
    }
  });
  it("closes the modal before handing a plan file link to the viewer", () => {
    const onOpenFileRef = vi.fn();
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(
      <AcpFileRefContext.Provider value={{ onOpenFileRef }}>
        <ApprovalCard approval={makeApproval({ plan: "see LINK" })} onResolve={onResolve} />
      </AcpFileRefContext.Provider>,
    );
    fireEvent.click(screen.getByText("View plan"));
    fireEvent.click(within(screen.getByRole("dialog")).getByText("plan file"));
    expect(onOpenFileRef).toHaveBeenCalledExactlyOnceWith({ path: "/repo/PLAN.md" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onResolve).not.toHaveBeenCalled();
  });
});

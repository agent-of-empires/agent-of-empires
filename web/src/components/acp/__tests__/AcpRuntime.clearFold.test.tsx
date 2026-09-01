// @vitest-environment jsdom
//
// #3640: a `/clear` moves the fold, which shortens the part list of a message
// the fold keeps and left assistant-ui's store reading a stale part index (the
// mechanism is on `RuntimeHost` in AcpRuntime). The fix rebuilds the runtime
// when the fold moves, so these tests cover the key and the rebuild it drives.
//
// The rendered failure is not asserted: jsdom swallows the throw and keeps the
// stale list (assistant-ui#5708), so it looks identical before and after the
// fix. The browser check is the manual procedure recorded on #3640.

import { act, render } from "@testing-library/react";
import { useEffect } from "react";
import { describe, expect, it, vi } from "vitest";

import { emptyAcpState } from "../../../lib/acpTypes";
import type { AcpState, ActivityRow } from "../../../lib/acpTypes";

const row = (id: string, kind: ActivityRow["kind"], text: string): ActivityRow => ({
  id,
  kind,
  text,
  at: "2026-08-31T12:00:00Z",
});

const TURNS: ActivityRow[] = [
  row("u1", "user_prompt", "first question"),
  row("a1", "message", "first answer"),
  row("u2", "user_prompt", "second question"),
  row("a2", "message", "second answer"),
];

// Reshaped between renders the way a `/clear` frame does. AcpRuntime reads
// `state` and hands the rest of the session straight to `children`, which these
// tests ignore, so a state-only stub is enough.
let activity: ActivityRow[] = TURNS;

vi.mock("../../../hooks/useAcpSession", () => ({
  useAcpSession: (): { state: AcpState } => ({ state: { ...emptyAcpState(), activity } }),
}));

import { AcpRuntime, clearFoldGeneration } from "../AcpRuntime";

describe("clearFoldGeneration (#3640)", () => {
  it("changes only when the fold point moves", () => {
    const cleared = [...TURNS, row("c1", "session_cleared", "cleared")];
    const cases: [string, ActivityRow[], boolean, string][] = [
      // No clear yet: one stable value for the whole session.
      ["no clear", TURNS, false, "none"],
      // A new turn on top does not move the fold point.
      ["turn appended", [...TURNS, row("a3", "message", "third answer")], false, "none"],
      // Cleared turns shown means no fold, so there is no truncation to guard.
      ["cleared shown", cleared, true, "all"],
      // Folded: pinned to the last clear, so each /clear yields a new value.
      ["folded", cleared, false, "c1"],
      ["folded twice", [...cleared, row("c2", "session_cleared", "cleared again")], false, "c2"],
    ];
    for (const [label, rows, showCleared, expected] of cases) {
      expect(clearFoldGeneration(rows, showCleared), label).toBe(expected);
    }
  });
});

describe("AcpRuntime: runtime rebuild on a fold change (#3640)", () => {
  it("rebuilds the runtime when a clear lands, and keeps it across an ordinary turn", async () => {
    // A rebuilt store remounts the provider subtree, so the child's own mount
    // count is the observable proof the store was replaced, not updated.
    let mounts = 0;
    const CountMounts = () => {
      useEffect(() => {
        mounts += 1;
      }, []);
      return null;
    };
    const Harness = () => <AcpRuntime sessionId="s1">{() => <CountMounts />}</AcpRuntime>;

    activity = TURNS;
    const { rerender } = render(<Harness />);
    const afterFirstRender = mounts;

    // An ordinary turn must not rebuild, or every turn would drop the composer
    // state and the scroll position.
    activity = [...TURNS, row("a3", "message", "third answer")];
    await act(async () => {
      rerender(<Harness />);
    });
    expect(mounts, "an ordinary turn must not rebuild the runtime").toBe(afterFirstRender);

    // The `/clear` divider moves the fold point, so the store must be replaced.
    activity = [...activity, row("c1", "session_cleared", "Conversation cleared")];
    await act(async () => {
      rerender(<Harness />);
    });
    expect(mounts, "a clear must rebuild the runtime").toBe(afterFirstRender + 1);
  });
});

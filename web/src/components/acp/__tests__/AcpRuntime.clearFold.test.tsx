// @vitest-environment jsdom
//
// #3640: `/clear` folds the pre-clear turns out of the transcript, which shortens
// the part list of a message the fold keeps. assistant-ui reads parts by absolute
// index, and its store keeps the committed list when a snapshot throws (upstream
// assistant-ui#5708). A stale part child then reads past the end, inside
// useSyncExternalStore, where React cannot recover, so the ErrorBoundary replaced
// the conversation view.
//
// The stale state sits in the store, so the runtime is rebuilt when the fold point
// moves. A remount of the message list alone was measured and did NOT stop the
// failure. These tests cover the fold key and the rebuild it drives.
//
// The rendered failure is not asserted here. In jsdom the throw is swallowed, so
// jsdom shows the same output before and after the fix. The browser check is the
// procedure recorded on #3640.

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

// The mocked session store reads this, so a test can reshape the transcript
// between renders the way a `/clear` frame does.
let activity: ActivityRow[] = TURNS;

vi.mock("../../../hooks/useAcpSession", () => ({
  useAcpSession: () => {
    const state: AcpState = { ...emptyAcpState(), activity };
    return {
      state,
      status: "open",
      hasEverOpened: true,
      reconnecting: false,
      retryCount: 0,
      retryCountdown: 0,
      maxRetries: 5,
      manualReconnect: () => {},
      resolveApproval: async () => {},
      resolveElicitation: async () => {},
      sendPrompt: async () => {},
      cancelPrompt: async () => {},
      forceEndTurn: async () => {},
      lastActivityRef: { current: 0 },
      dismissError: () => {},
      dismissPrimer: () => {},
      dismissCompactionReminder: () => {},
      removeQueuedPrompt: () => {},
      editQueuedPrompt: () => {},
      clearQueue: () => {},
      sendQueuedNow: async () => {},
      canSendQueuedNow: false,
      sendNowInterruptsTurn: false,
      dismissRejectedPrompt: () => {},
      dismissModeSwitchFailed: () => {},
      setConfigOption: async () => {},
      dismissConfigOptionSwitchFailed: () => {},
      loadOlder: async () => {},
      hasMoreOlder: false,
      loadingOlder: false,
    };
  },
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
    // A fresh external store means a fresh mount of the provider subtree. The
    // child counts its own mounts, which is what the rebuild is for: the stale
    // store is what throws, so it must be replaced, not updated.
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

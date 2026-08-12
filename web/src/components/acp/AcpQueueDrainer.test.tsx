// @vitest-environment jsdom
//
// Which sessions get a headless drain subscription (#3331). Each one costs
// a WebSocket and a replay top-up, so the selection rules are the point:
// structured sessions only, never the chat already on screen, never a
// trashed session, only armed queues, capped at MAX_BACKGROUND_DRAINERS.
//
// `useAcpSession` is mocked to a spy: this asserts the selection, not the
// subscription machinery (covered in useAcpSession.queue.test.ts).

import { render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AcpBackgroundDrainers } from "./AcpQueueDrainer";
import { __resetDrainCoordinator, armForBackgroundDrain } from "../../lib/acpDrainCoordinator";
import { MAX_BACKGROUND_DRAINERS } from "../../hooks/useAcpQueueCount";
import { setQueueCount } from "../../lib/acpStateStorage";
import type { SessionResponse } from "../../lib/types";

const subscribed = vi.fn();

vi.mock("../../hooks/useAcpSession", () => ({
  useAcpSession: (sessionId: string) => {
    subscribed(sessionId);
    return {};
  },
}));

function session(id: string, over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id,
    title: id,
    tool: "claude",
    view: "structured",
    trashed_at: null,
    archived_at: null,
    snoozed_until: null,
    acp_worker_state: "running",
    ...over,
  } as SessionResponse;
}

/** Give a session a queue and arm it, the state a parked prompt leaves. */
function queued(id: string): void {
  setQueueCount(id, 1);
  armForBackgroundDrain(id);
}

beforeEach(() => {
  subscribed.mockClear();
  window.localStorage.clear();
  __resetDrainCoordinator();
});

afterEach(() => {
  __resetDrainCoordinator();
});

describe("AcpBackgroundDrainers", () => {
  it("subscribes only to armed, queued, non-active structured sessions", () => {
    const sessions = [
      session("with-queue"),
      session("active-with-queue"),
      session("no-queue"),
      session("terminal-with-queue", { view: "terminal" }),
      session("trashed-with-queue", { trashed_at: "2026-08-12T00:00:00Z" }),
      session("unarmed-with-queue"),
    ];
    for (const id of ["with-queue", "active-with-queue", "terminal-with-queue", "trashed-with-queue"]) {
      queued(id);
    }
    // Queued but never armed: a stale queue restored from storage. It keeps
    // its badge and waits for the user to open that chat.
    setQueueCount("unarmed-with-queue", 1);

    render(<AcpBackgroundDrainers sessions={sessions} activeSessionId="active-with-queue" />);

    expect(subscribed.mock.calls.map((c) => c[0])).toEqual(["with-queue"]);
  });

  it("renders no DOM and caps concurrent subscriptions", () => {
    const ids = Array.from({ length: MAX_BACKGROUND_DRAINERS + 2 }, (_, i) => `s${i}`);
    for (const id of ids) queued(id);

    const { container } = render(
      <AcpBackgroundDrainers sessions={ids.map((id) => session(id))} activeSessionId={null} />,
    );

    // Over the cap: the extras keep their queue and drain once a slot frees
    // or the user opens them, rather than opening a socket each.
    expect(subscribed).toHaveBeenCalledTimes(MAX_BACKGROUND_DRAINERS);
    expect(container.innerHTML).toBe("");
  });
});

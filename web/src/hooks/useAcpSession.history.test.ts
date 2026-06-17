// @vitest-environment jsdom
//
// Reducer tests for recent-first history paging (#2236): the `prepend`
// action must add older rows without clobbering live/optimistic state,
// the `frames` tail seeds the oldestSeq watermark, and `handshake` only
// backfills empty fields.

import { describe, expect, it } from "vitest";

import { emptyAcpState, type AcpFrame } from "../lib/acpTypes";
import { reducer } from "./useAcpSession";

function prompt(seq: number, text: string): AcpFrame {
  return { session_id: "s", seq, event: { UserPromptSent: { text } } };
}

describe("useAcpSession recent-first paging", () => {
  it("frames tail seeds oldestSeq once; live appends don't lower it", () => {
    let s = reducer(emptyAcpState(), {
      kind: "frames",
      frames: [prompt(5, "e"), prompt(6, "f")],
      oldestSeq: 5,
    });
    expect(s.oldestSeq).toBe(5);
    // A later live batch (no oldestSeq) must not move the floor.
    s = reducer(s, { kind: "frames", frames: [prompt(7, "g")] });
    expect(s.oldestSeq).toBe(5);
  });

  it("prepend adds older rows ahead of the loaded tail and lowers oldestSeq", () => {
    let s = reducer(emptyAcpState(), {
      kind: "frames",
      frames: [prompt(5, "e"), prompt(6, "f")],
      oldestSeq: 5,
    });
    s = reducer(s, { kind: "prepend", frames: [prompt(2, "b"), prompt(3, "c")], oldestSeq: 2 });
    expect(s.oldestSeq).toBe(2);
    expect(s.activity.map((r) => r.id)).toEqual(["user-seq-2", "user-seq-3", "user-seq-5", "user-seq-6"]);
  });

  it("prepend preserves optimistic / queue / approval state", () => {
    let s = reducer(emptyAcpState(), { kind: "frames", frames: [prompt(5, "e")], oldestSeq: 5 });
    s = reducer(s, { kind: "enqueue_prompt", text: "queued" });
    // A pending approval the server hasn't echoed a resolution for yet
    // (the #1821 optimistic case the prepend must not disturb). Only the
    // reference matters here, so a minimal stand-in is enough.
    const approvals = [{ nonce: "n1" } as unknown as (typeof s.pendingApprovals)[number]];
    s = { ...s, pendingApprovals: approvals };

    s = reducer(s, { kind: "prepend", frames: [prompt(2, "b")], oldestSeq: 2 });

    expect(s.queuedPrompts).toHaveLength(1);
    expect(s.queuedPrompts[0]!.text).toBe("queued");
    // Same reference: the prepend never re-folded the log, so the
    // optimistic approval survives untouched.
    expect(s.pendingApprovals).toBe(approvals);
    expect(s.activity[0]!.id).toBe("user-seq-2");
  });

  it("handshake backfills empty fields but never overwrites loaded values", () => {
    const caps: AcpFrame = {
      session_id: "s",
      seq: 1,
      event: { PromptCapabilities: { image: true, audio: false, embedded_context: true } },
    };
    // promptCapabilities already established by the tail must win over the
    // (older) handshake snapshot.
    const withCaps = {
      ...emptyAcpState(),
      promptCapabilities: { image: false, audio: false, embeddedContext: false },
    };
    const kept = reducer(withCaps, { kind: "handshake", frames: [caps] });
    expect(kept.promptCapabilities).toEqual({ image: false, audio: false, embeddedContext: false });

    // When the field is still empty, the handshake fills it.
    const filled = reducer(emptyAcpState(), { kind: "handshake", frames: [caps] });
    expect(filled.promptCapabilities).toEqual({ image: true, audio: false, embeddedContext: true });
    // The handshake projects state only; it adds no transcript rows.
    expect(filled.activity).toHaveLength(0);
  });
});

// Reducer tests for structured view attachment support (#1000 / #965).
//
// Cover the wire-protocol contract the composer and replay depend on:
// the PromptCapabilities event drives the composer's attachment gate,
// and a UserPromptSent carrying attachment refs maps each ref to a
// render-ready attachment backed by the replay GET endpoint. If either
// regresses, the paperclip silently disables or replayed screenshots
// fail to render.

import { describe, expect, it } from "vitest";

import { applyEvent, emptyAcpState, transcriptRowToActivity, type AcpFrame } from "./acpTypes";

describe("structured view attachments reducer", () => {
  it("stores prompt capabilities from the PromptCapabilities event", () => {
    const frame: AcpFrame = {
      session_id: "s-1",
      seq: 1,
      event: {
        PromptCapabilities: {
          image: true,
          audio: false,
          embedded_context: true,
        },
      },
    };
    const next = applyEvent(emptyAcpState(), frame);
    expect(next.promptCapabilities).toEqual({
      image: true,
      audio: false,
      embeddedContext: true,
      steering: false,
    });
  });

  it("re-emits supersede earlier capabilities (agent switch)", () => {
    let state = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        PromptCapabilities: {
          image: true,
          audio: true,
          embedded_context: true,
        },
      },
    });
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 2,
      event: {
        PromptCapabilities: {
          image: false,
          audio: false,
          embedded_context: false,
        },
      },
    });
    expect(state.promptCapabilities).toEqual({
      image: false,
      audio: false,
      embeddedContext: false,
      steering: false,
    });
  });

  it("maps server attachment refs to a GET-backed url on the transcript row", () => {
    // The transcript is server-owned (Tier 4): the daemon carries attachment
    // refs on the `user_prompt` TranscriptRow, and the client maps each to a
    // replay-GET-backed AcpAttachment via `transcriptRowToActivity`.
    const row = transcriptRowToActivity(
      {
        id: "user-seq-5",
        group_id: "g1",
        kind: "user_prompt",
        at: "2026-01-01T00:00:00Z",
        text: "what is wrong here?",
        attachments: [{ id: "att-abc", kind: "image", mime_type: "image/png", name: "shot.png", size: 1234 }],
      },
      "sess-42",
    );
    expect(row.attachments).toHaveLength(1);
    expect(row.attachments?.[0]).toEqual({
      id: "att-abc",
      kind: "image",
      mimeType: "image/png",
      name: "shot.png",
      size: 1234,
      url: "/api/sessions/sess-42/acp/attachments/att-abc",
    });
  });

  it("leaves attachments undefined on a text-only transcript row", () => {
    const row = transcriptRowToActivity(
      { id: "user-seq-1", group_id: "g1", kind: "user_prompt", at: "2026-01-01T00:00:00Z", text: "plain" },
      "s-1",
    );
    expect(row.attachments).toBeUndefined();
  });
});

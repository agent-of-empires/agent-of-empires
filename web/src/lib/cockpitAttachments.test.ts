// Reducer tests for cockpit attachment support (#1000 / #965).
//
// Cover the wire-protocol contract the composer and replay depend on:
// the PromptCapabilities event drives the composer's attachment gate,
// and a UserPromptSent carrying attachment refs maps each ref to a
// render-ready attachment backed by the replay GET endpoint. If either
// regresses, the paperclip silently disables or replayed screenshots
// fail to render.

import { describe, expect, it } from "vitest";

import {
  applyEvent,
  emptyCockpitState,
  type CockpitFrame,
} from "./cockpitTypes";

describe("cockpit attachments reducer", () => {
  it("stores prompt capabilities from the PromptCapabilities event", () => {
    const frame: CockpitFrame = {
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
    const next = applyEvent(emptyCockpitState(), frame);
    expect(next.promptCapabilities).toEqual({
      image: true,
      audio: false,
      embeddedContext: true,
    });
  });

  it("re-emits supersede earlier capabilities (agent switch)", () => {
    let state = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: {
        PromptCapabilities: { image: true, audio: true, embedded_context: true },
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
    });
  });

  it("maps server attachment refs to a GET-backed url on the user row", () => {
    const frame: CockpitFrame = {
      session_id: "sess-42",
      seq: 5,
      event: {
        UserPromptSent: {
          text: "what is wrong here?",
          attachments: [
            {
              id: "att-abc",
              kind: "image",
              mime_type: "image/png",
              name: "shot.png",
              size: 1234,
            },
          ],
        },
      },
    };
    const next = applyEvent(emptyCockpitState(), frame);
    const row = next.activity.find((r) => r.kind === "user_prompt");
    expect(row).toBeDefined();
    expect(row?.attachments).toHaveLength(1);
    expect(row?.attachments?.[0]).toEqual({
      id: "att-abc",
      kind: "image",
      mimeType: "image/png",
      name: "shot.png",
      size: 1234,
      url: "/api/sessions/sess-42/cockpit/attachments/att-abc",
    });
  });

  it("leaves attachments undefined on a text-only prompt", () => {
    const next = applyEvent(emptyCockpitState(), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "plain" } },
    });
    const row = next.activity.find((r) => r.kind === "user_prompt");
    expect(row?.attachments).toBeUndefined();
  });
});

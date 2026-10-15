import { describe, expect, it } from "vitest";

import { transcriptRowToActivity } from "./acpTypes";

describe("structured view attachments reducer", () => {
  it("maps server attachment refs to a GET-backed url on the transcript row", () => {
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

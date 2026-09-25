// @vitest-environment jsdom

import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { updateProfileSettings } from "../../../lib/api";
import type { SettingsFieldDescriptor } from "../../../lib/types";
import { SchemaSection } from "../SchemaSection";

const NEW_SESSION_MODE_FIELD = {
  section: "session",
  field: "new_session_mode",
  category: "Session",
  label: "New Session Mode",
  description: "How the TUI opens a terminal-mode session after it is created.",
  widget: {
    kind: "select",
    options: [
      { value: "match_default", label: "Match default attach" },
      { value: "tmux", label: "Tmux" },
      { value: "live_send", label: "Live mode" },
    ],
  },
  web_write: { policy: "allow" },
  profile_overridable: true,
  validation: { rule: "none" },
  advanced: false,
} satisfies SettingsFieldDescriptor;

const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

describe("New Session Mode setting", () => {
  it("sends the selected mode in the profile settings PATCH", async () => {
    let receivedBody: unknown;
    server.use(
      http.get("*/api/settings/schema", () => HttpResponse.json([NEW_SESSION_MODE_FIELD])),
      http.patch("*/api/profiles/main/settings", async ({ request }) => {
        receivedBody = await request.json();
        return new HttpResponse(null, { status: 204 });
      }),
    );

    const { container } = render(
      <SchemaSection
        section="session"
        schema={[NEW_SESSION_MODE_FIELD]}
        values={{}}
        onSaveField={(_section, field, value) => updateProfileSettings("main", { session: { [field]: value } })}
      />,
    );
    await screen.findByText("New Session Mode");
    const select = container.querySelector("select");
    expect(select).not.toBeNull();

    fireEvent.change(select!, { target: { value: "live_send" } });

    await waitFor(() => expect(receivedBody).toEqual({ session: { new_session_mode: "live_send" } }));
  });
});

// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { SendButton } from "./ComposerControls";

afterEach(cleanup);

describe("SendButton", () => {
  it.each<[string, boolean, string | null, string, string]>([
    ["connected", true, null, "Send message", "Send, Enter"],
    ["connected while its Agent starts", true, "Claude Code", "Send message", "Send, Enter"],
    [
      "a new Session waits for its Agent",
      false,
      "Claude Code",
      "Pending until Claude Code is ready",
      "Pending until Claude Code is ready",
    ],
    [
      "a resumed Session waits to resume",
      false,
      null,
      "Queue message until session resumes",
      "Session not active, will send on resume",
    ],
  ])("%s", (_label, connected, waitingFor, name, title) => {
    render(<SendButton connected={connected} waitingFor={waitingFor} onSend={() => {}} />);
    expect(screen.getByRole("button", { name }).getAttribute("title")).toBe(title);
  });
});

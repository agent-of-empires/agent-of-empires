// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { QueueSendButton, SendButton } from "./ComposerControls";

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

describe("QueueSendButton", () => {
  it.each<[string, boolean, boolean, string | null, string, string]>([
    [
      "new Session pending",
      false,
      false,
      "Claude Code",
      "Pending until Claude Code is ready",
      "Pending until Claude Code is ready",
    ],
    [
      "resumed Session pending",
      false,
      false,
      null,
      "Queue follow-up message",
      "Queue follow-up, will send on resume, Enter",
    ],
    [
      "connected steering",
      true,
      true,
      "Claude Code",
      "Send message into the current turn",
      "Send into the current turn, Enter",
    ],
    [
      "connected queue",
      true,
      false,
      "Claude Code",
      "Queue follow-up message",
      "Queue follow-up (sent when current turn ends), Enter",
    ],
  ])("%s", (_label, connected, steering, waitingFor, name, title) => {
    render(<QueueSendButton connected={connected} steering={steering} waitingFor={waitingFor} onSend={() => {}} />);
    expect(screen.getByRole("button", { name }).getAttribute("title")).toBe(title);
  });
});

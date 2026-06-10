// @vitest-environment jsdom
//
// AskUserQuestion card rendering + resolution routing. Pins:
//   - single-select renders radios; multi-select renders checkboxes;
//     free-text renders a text input,
//   - Submit sends `accept` with the chosen labels (single as string,
//     multi as array, free-text as string),
//   - a required, unanswered question blocks Submit with a validation
//     message and no onResolve call,
//   - Skip sends `decline`, Cancel sends `cancel`.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { AskUserQuestionCard } from "./AskUserQuestionCard";
import type { Elicitation, ElicitationQuestion } from "../../lib/acpTypes";

vi.mock("../../lib/connectionState", () => ({
  useServerDown: () => false,
  OFFLINE_TITLE: "Disconnected",
}));

function makeElicitation(questions: ElicitationQuestion[], message = "Pick"): Elicitation {
  return {
    nonce: "e-1",
    message,
    tool_call_id: null,
    questions,
    requested_at: "2026-06-10T00:00:00Z",
    resolved: null,
  };
}

const singleSelect: ElicitationQuestion = {
  field_key: "question_0",
  title: "Color?",
  description: null,
  required: true,
  kind: "single_select",
  options: [
    { value: "Red", label: "Red" },
    { value: "Blue", label: "Blue" },
  ],
  min_items: null,
  max_items: null,
};

afterEach(() => cleanup());

describe("AskUserQuestionCard", () => {
  it("renders question chrome and single-select radios", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<AskUserQuestionCard elicitation={makeElicitation([singleSelect])} onResolve={onResolve} />);
    expect(screen.getByRole("alertdialog", { name: /Question from the agent/i })).toBeTruthy();
    const radios = screen.getAllByRole("radio");
    expect(radios).toHaveLength(2);
  });

  it("submits a single-select answer as a string label", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<AskUserQuestionCard elicitation={makeElicitation([singleSelect])} onResolve={onResolve} />);
    fireEvent.click(screen.getByLabelText("Blue"));
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).toHaveBeenCalledWith({
      action: "accept",
      answers: { question_0: "Blue" },
    });
  });

  it("submits a multi-select answer as an array of labels", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const multi: ElicitationQuestion = {
      field_key: "question_0",
      title: "Toppings",
      description: null,
      required: false,
      kind: "multi_select",
      options: [
        { value: "a", label: "Anchovy" },
        { value: "b", label: "Basil" },
      ],
      min_items: null,
      max_items: null,
    };
    render(<AskUserQuestionCard elicitation={makeElicitation([multi])} onResolve={onResolve} />);
    fireEvent.click(screen.getByLabelText("Anchovy"));
    fireEvent.click(screen.getByLabelText("Basil"));
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).toHaveBeenCalledWith({
      action: "accept",
      answers: { question_0: ["a", "b"] },
    });
  });

  it("submits free text", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const free: ElicitationQuestion = {
      field_key: "customAnswer",
      title: "Other",
      description: null,
      required: false,
      kind: "free_text",
      options: [],
      min_items: null,
      max_items: null,
    };
    render(<AskUserQuestionCard elicitation={makeElicitation([free])} onResolve={onResolve} />);
    fireEvent.change(screen.getByPlaceholderText("Type your answer"), {
      target: { value: "purple" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).toHaveBeenCalledWith({
      action: "accept",
      answers: { customAnswer: "purple" },
    });
  });

  it("blocks Submit when a required question is unanswered", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<AskUserQuestionCard elicitation={makeElicitation([singleSelect])} onResolve={onResolve} />);
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).not.toHaveBeenCalled();
    expect(screen.getByText(/Please answer/i)).toBeTruthy();
  });

  it("Skip declines", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<AskUserQuestionCard elicitation={makeElicitation([singleSelect])} onResolve={onResolve} />);
    fireEvent.click(screen.getByRole("button", { name: "Skip" }));
    expect(onResolve).toHaveBeenCalledWith({ action: "decline" });
  });

  it("Cancel cancels", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    render(<AskUserQuestionCard elicitation={makeElicitation([singleSelect])} onResolve={onResolve} />);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onResolve).toHaveBeenCalledWith({ action: "cancel" });
  });
});

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

/** Build a question with the optional schema fields defaulted off. */
function q(
  partial: Partial<ElicitationQuestion> & Pick<ElicitationQuestion, "field_key" | "kind">,
): ElicitationQuestion {
  return { title: null, description: null, required: false, options: [], ...partial };
}

describe("AskUserQuestionCard extended field kinds", () => {
  it("renders a number field, submits a number, and range-checks", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const num = q({ field_key: "question_0", kind: "number", title: "Temp", minimum: 0, maximum: 10 });
    render(<AskUserQuestionCard elicitation={makeElicitation([num])} onResolve={onResolve} />);
    const input = screen.getByPlaceholderText("Enter a number");
    fireEvent.change(input, { target: { value: "99" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).not.toHaveBeenCalled();
    expect(screen.getByText(/must be at most 10/i)).toBeTruthy();
    fireEvent.change(input, { target: { value: "5" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { question_0: 5 } });
  });

  it("rejects a fractional value for an integer field", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const int = q({ field_key: "question_0", kind: "integer", title: "Count" });
    render(<AskUserQuestionCard elicitation={makeElicitation([int])} onResolve={onResolve} />);
    fireEvent.change(screen.getByPlaceholderText("Enter a number"), { target: { value: "3.5" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).not.toHaveBeenCalled();
    expect(screen.getByText(/must be a whole number/i)).toBeTruthy();
  });

  it("renders a boolean checkbox and always submits a boolean", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const bool = q({ field_key: "question_0", kind: "boolean", title: "Enable telemetry" });
    render(<AskUserQuestionCard elicitation={makeElicitation([bool])} onResolve={onResolve} />);
    // Untouched optional boolean still submits its definite (false) state.
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).toHaveBeenLastCalledWith({ action: "accept", answers: { question_0: false } });
    cleanup();
    onResolve.mockClear();
    render(<AskUserQuestionCard elicitation={makeElicitation([bool])} onResolve={onResolve} />);
    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).toHaveBeenLastCalledWith({ action: "accept", answers: { question_0: true } });
  });

  it("validates free-text min length", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const free = q({ field_key: "question_0", kind: "free_text", title: "Name", min_length: 3 });
    render(<AskUserQuestionCard elicitation={makeElicitation([free])} onResolve={onResolve} />);
    fireEvent.change(screen.getByPlaceholderText("Type your answer"), { target: { value: "ab" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).not.toHaveBeenCalled();
    expect(screen.getByText(/at least 3 characters/i)).toBeTruthy();
  });

  it("renders an email field as a typed input", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const email = q({ field_key: "question_0", kind: "free_text", title: "Email", format: "email" });
    render(<AskUserQuestionCard elicitation={makeElicitation([email])} onResolve={onResolve} />);
    expect(screen.getByPlaceholderText("Type your answer").getAttribute("type")).toBe("email");
  });

  it("blocks Submit on a malformed email and accepts a valid one", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const email = q({ field_key: "question_0", kind: "free_text", title: "Email", format: "email" });
    render(<AskUserQuestionCard elicitation={makeElicitation([email])} onResolve={onResolve} />);
    const input = screen.getByPlaceholderText("Type your answer");
    fireEvent.change(input, { target: { value: "notanemail" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).not.toHaveBeenCalled();
    expect(screen.getByText(/is not a valid email/i)).toBeTruthy();
    fireEvent.change(input, { target: { value: "a@b.co" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { question_0: "a@b.co" } });
  });

  it("splits the adapter's flattened option label into a two-tier label + description", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    // The adapter flattens to `"<label> — <description>"` keeping the bare
    // label as the option value.
    const sel = q({
      field_key: "question_0",
      kind: "single_select",
      title: "Pick",
      options: [{ value: "Red", label: "Red — the warm one" }],
    });
    render(<AskUserQuestionCard elicitation={makeElicitation([sel])} onResolve={onResolve} />);
    expect(screen.getByText("Red")).toBeTruthy();
    expect(screen.getByText("the warm one")).toBeTruthy();
    fireEvent.click(screen.getAllByRole("radio")[0]!);
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { question_0: "Red" } });
  });

  it("pre-fills defaults across kinds", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const questions = [
      q({
        field_key: "question_0",
        kind: "single_select",
        title: "Color",
        options: [
          { value: "Red", label: "Red" },
          { value: "Blue", label: "Blue" },
        ],
        default: "Blue",
      }),
      q({ field_key: "question_1", kind: "number", title: "N", default: 5 }),
      q({ field_key: "question_2", kind: "boolean", title: "On", default: true }),
    ];
    render(<AskUserQuestionCard elicitation={makeElicitation(questions)} onResolve={onResolve} />);
    expect((screen.getByRole("radio", { name: "Blue" }) as HTMLInputElement).checked).toBe(true);
    expect((screen.getByPlaceholderText("Enter a number") as HTMLInputElement).value).toBe("5");
    expect((screen.getByRole("checkbox") as HTMLInputElement).checked).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(onResolve).toHaveBeenCalledWith({
      action: "accept",
      answers: { question_0: "Blue", question_1: 5, question_2: true },
    });
  });

  it("renders schema-level title and description", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const elicitation: Elicitation = {
      ...makeElicitation([singleSelect]),
      title: "Your profile",
      description: "These help tailor the result.",
    };
    render(<AskUserQuestionCard elicitation={elicitation} onResolve={onResolve} />);
    expect(screen.getByText("Your profile")).toBeTruthy();
    expect(screen.getByText("These help tailor the result.")).toBeTruthy();
  });

  it("renders the full prompt without truncation", () => {
    const onResolve = vi.fn().mockResolvedValue(undefined);
    const longMessage =
      "This is a deliberately long question that should wrap onto multiple lines rather than being clipped by a truncate class in the header bar.";
    render(<AskUserQuestionCard elicitation={makeElicitation([singleSelect], longMessage)} onResolve={onResolve} />);
    const prompt = screen.getByText(longMessage);
    expect(prompt.className).toContain("whitespace-pre-wrap");
    expect(prompt.className).not.toContain("truncate");
  });
});

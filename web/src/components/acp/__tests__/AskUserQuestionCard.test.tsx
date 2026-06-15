// @vitest-environment jsdom
//
// Branch-coverage tests for AskUserQuestionCard: one suite per
// ElicitationFieldKind plus the multi-question, decline/cancel, validation,
// and offline/rollback branches. Each kind exercises render -> interact ->
// submit and asserts the ElicitationResolution payload shape.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { AskUserQuestionCard } from "../AskUserQuestionCard";
import { setServerDown } from "../../../lib/connectionState";
import type { Elicitation, ElicitationQuestion, ElicitationResolution } from "../../../lib/acpTypes";

function makeQuestion(overrides: Partial<ElicitationQuestion> & { field_key: string }): ElicitationQuestion {
  return {
    title: null,
    description: null,
    required: false,
    kind: "free_text",
    options: [],
    min_items: null,
    max_items: null,
    min_length: null,
    max_length: null,
    pattern: null,
    format: null,
    minimum: null,
    maximum: null,
    default: null,
    ...overrides,
  };
}

function makeElicitation(questions: ElicitationQuestion[], overrides: Partial<Elicitation> = {}): Elicitation {
  return {
    nonce: "nonce-1",
    message: "Please answer the question",
    title: null,
    description: null,
    tool_call_id: null,
    questions,
    requested_at: "2026-01-01T00:00:00Z",
    resolved: null,
    ...overrides,
  };
}

function renderCard(elicitation: Elicitation) {
  const onResolve = vi.fn<(r: ElicitationResolution) => Promise<void>>().mockResolvedValue(undefined);
  const utils = render(<AskUserQuestionCard elicitation={elicitation} onResolve={onResolve} />);
  return { onResolve, ...utils };
}

const clickSubmit = () => fireEvent.click(screen.getByRole("button", { name: "Submit" }));
const typeText = (el: HTMLElement, value: string) => fireEvent.change(el, { target: { value } });
const textInput = () => screen.getByPlaceholderText("Type your answer") as HTMLInputElement;
const numberInput = () => screen.getByPlaceholderText("Enter a number") as HTMLInputElement;
const checkboxByName = (name: string) => screen.getByRole("checkbox", { name }) as HTMLInputElement;

beforeEach(() => {
  setServerDown(false);
});

afterEach(() => {
  setServerDown(false);
  cleanup();
});

describe("AskUserQuestionCard free_text", () => {
  it("submits typed text as a string answer", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "name", title: "Your name", kind: "free_text" })]),
    );
    typeText(textInput(), "Ada");
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { name: "Ada" } });
  });

  it("seeds the default string value and omits empty optional answers", () => {
    const { onResolve } = renderCard(
      makeElicitation([
        makeQuestion({ field_key: "seeded", kind: "free_text", default: "preset" }),
        makeQuestion({ field_key: "blank", kind: "free_text" }),
      ]),
    );
    const inputs = screen.getAllByPlaceholderText("Type your answer") as HTMLInputElement[];
    expect(inputs[0]?.value).toBe("preset");
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { seeded: "preset" } });
  });

  it("blocks submit when a required free_text is empty", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "req", title: "Required", required: true, kind: "free_text" })]),
    );
    clickSubmit();
    expect(onResolve).not.toHaveBeenCalled();
    expect(screen.getByText("Please answer: Required")).toBeTruthy();
  });

  it("validates email format", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "mail", title: "Email", kind: "free_text", format: "email" })]),
    );
    typeText(textInput(), "not-an-email");
    clickSubmit();
    expect(onResolve).not.toHaveBeenCalled();
    expect(screen.getByText("Email is not a valid email")).toBeTruthy();
  });

  it("enforces min_length / max_length and pattern", () => {
    const { onResolve } = renderCard(
      makeElicitation([
        makeQuestion({
          field_key: "code",
          title: "Code",
          kind: "free_text",
          min_length: 3,
          max_length: 10,
          pattern: "^[a-z]+$",
        }),
      ]),
    );
    const input = textInput();
    typeText(input, "ab");
    clickSubmit();
    expect(screen.getByText("Code must be at least 3 characters")).toBeTruthy();
    expect(onResolve).not.toHaveBeenCalled();

    typeText(input, "AB12");
    clickSubmit();
    expect(screen.getByText("Code does not match the required format")).toBeTruthy();

    typeText(input, "abc");
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { code: "abc" } });
  });

  it("ignores an unparseable pattern (treated as no constraint)", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "any", kind: "free_text", pattern: "([" })]),
    );
    typeText(textInput(), "whatever");
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { any: "whatever" } });
  });

  it("flags a too-long value against max_length", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "code", title: "Code", kind: "free_text", max_length: 3 })]),
    );
    typeText(textInput(), "toolong");
    clickSubmit();
    expect(screen.getByText("Code must be at most 3 characters")).toBeTruthy();
    expect(onResolve).not.toHaveBeenCalled();
  });
});

describe("AskUserQuestionCard single_select", () => {
  const question = makeQuestion({
    field_key: "color",
    title: "Pick a color",
    kind: "single_select",
    options: [
      { value: "red", label: "red" },
      { value: "green", label: "green" },
    ],
  });

  it("submits the chosen radio value", () => {
    const { onResolve } = renderCard(makeElicitation([question]));
    fireEvent.click(screen.getByRole("radio", { name: /green/ }));
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { color: "green" } });
  });

  it("renders the two-tier label/description recovered from the title separator", () => {
    renderCard(
      makeElicitation([
        makeQuestion({
          field_key: "opt",
          kind: "single_select",
          options: [{ value: "fast", label: "fast — quickest option" }],
        }),
      ]),
    );
    expect(screen.getByText("fast")).toBeTruthy();
    expect(screen.getByText("quickest option")).toBeTruthy();
  });

  it("blocks submit on a required single_select with nothing chosen", () => {
    const { onResolve } = renderCard(makeElicitation([{ ...question, required: true }]));
    clickSubmit();
    expect(onResolve).not.toHaveBeenCalled();
    expect(screen.getByText("Please answer: Pick a color")).toBeTruthy();
  });
});

describe("AskUserQuestionCard multi_select", () => {
  const options = [
    { value: "a", label: "a" },
    { value: "b", label: "b" },
    { value: "c", label: "c" },
  ];

  it("accumulates and deselects checkbox values", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "tags", title: "Tags", kind: "multi_select", options })]),
    );
    fireEvent.click(screen.getByRole("checkbox", { name: "a" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "b" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "a" })); // deselect a
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { tags: ["b"] } });
  });

  it("seeds defaults from an array", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "tags", kind: "multi_select", options, default: ["a", "c"] })]),
    );
    expect(checkboxByName("a").checked).toBe(true);
    expect(checkboxByName("c").checked).toBe(true);
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { tags: ["a", "c"] } });
  });

  it("omits an empty optional multi_select from the payload", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "tags", kind: "multi_select", options })]),
    );
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: {} });
  });

  it("enforces required, min_items, and max_items", () => {
    const { onResolve } = renderCard(
      makeElicitation([
        makeQuestion({
          field_key: "tags",
          title: "Tags",
          kind: "multi_select",
          required: true,
          min_items: 2,
          max_items: 2,
          options,
        }),
      ]),
    );
    clickSubmit();
    expect(screen.getByText("Please answer: Tags")).toBeTruthy();

    fireEvent.click(screen.getByRole("checkbox", { name: "a" }));
    clickSubmit();
    expect(screen.getByText("Select at least 2 for Tags")).toBeTruthy();

    fireEvent.click(screen.getByRole("checkbox", { name: "b" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "c" }));
    clickSubmit();
    expect(screen.getByText("Select at most 2 for Tags")).toBeTruthy();
    expect(onResolve).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("checkbox", { name: "c" }));
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { tags: ["a", "b"] } });
  });
});

describe("AskUserQuestionCard number / integer", () => {
  it("submits a number value", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "qty", title: "Qty", kind: "number" })]),
    );
    typeText(numberInput(), "3.5");
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { qty: 3.5 } });
  });

  it("seeds a numeric default", () => {
    const { onResolve } = renderCard(makeElicitation([makeQuestion({ field_key: "qty", kind: "number", default: 7 })]));
    expect(numberInput().value).toBe("7");
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { qty: 7 } });
  });

  it("skips an empty optional number", () => {
    const { onResolve } = renderCard(makeElicitation([makeQuestion({ field_key: "qty", kind: "number" })]));
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: {} });
  });

  it("blocks an empty required number", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "qty", title: "Qty", kind: "number", required: true })]),
    );
    clickSubmit();
    expect(onResolve).not.toHaveBeenCalled();
    expect(screen.getByText("Please answer: Qty")).toBeTruthy();
  });

  it("enforces minimum and maximum", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "qty", title: "Qty", kind: "number", minimum: 1, maximum: 5 })]),
    );
    const input = numberInput();
    typeText(input, "0");
    clickSubmit();
    expect(screen.getByText("Qty must be at least 1")).toBeTruthy();

    typeText(input, "9");
    clickSubmit();
    expect(screen.getByText("Qty must be at most 5")).toBeTruthy();
    expect(onResolve).not.toHaveBeenCalled();
  });

  it("rejects a non-integer for an integer field and accepts a whole number", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "count", title: "Count", kind: "integer" })]),
    );
    const input = numberInput();
    typeText(input, "2.5");
    clickSubmit();
    expect(screen.getByText("Count must be a whole number")).toBeTruthy();
    expect(onResolve).not.toHaveBeenCalled();

    typeText(input, "4");
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { count: 4 } });
  });
});

describe("AskUserQuestionCard boolean", () => {
  it("submits false when left unchecked", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "agree", title: "Agree?", kind: "boolean" })]),
    );
    const checkbox = screen.getByRole("checkbox") as HTMLInputElement;
    expect(checkbox.checked).toBe(false);
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { agree: false } });
  });

  it("submits true when toggled on", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "agree", title: "Agree?", kind: "boolean" })]),
    );
    fireEvent.click(screen.getByRole("checkbox"));
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { agree: true } });
  });

  it("seeds a true default", () => {
    const { onResolve } = renderCard(
      makeElicitation([makeQuestion({ field_key: "agree", kind: "boolean", default: true })]),
    );
    expect((screen.getByRole("checkbox") as HTMLInputElement).checked).toBe(true);
    clickSubmit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { agree: true } });
  });
});

describe("AskUserQuestionCard multiple questions", () => {
  it("submits one payload covering every kind", () => {
    const { onResolve } = renderCard(
      makeElicitation(
        [
          makeQuestion({ field_key: "name", title: "Name", kind: "free_text" }),
          makeQuestion({
            field_key: "color",
            title: "Color",
            kind: "single_select",
            options: [{ value: "blue", label: "blue" }],
          }),
          makeQuestion({
            field_key: "tags",
            title: "Tags",
            kind: "multi_select",
            options: [
              { value: "x", label: "x" },
              { value: "y", label: "y" },
            ],
          }),
          makeQuestion({ field_key: "qty", title: "Qty", kind: "number" }),
          makeQuestion({ field_key: "agree", title: "Agree", kind: "boolean" }),
        ],
        { title: "Form heading", description: "Form description" },
      ),
    );
    expect(screen.getByText("Form heading")).toBeTruthy();
    expect(screen.getByText("Form description")).toBeTruthy();

    typeText(textInput(), "Grace");
    fireEvent.click(screen.getByRole("radio", { name: /blue/ }));
    fireEvent.click(screen.getByRole("checkbox", { name: "x" }));
    typeText(numberInput(), "2");
    fireEvent.click(screen.getByRole("checkbox", { name: "Agree" }));
    clickSubmit();

    expect(onResolve).toHaveBeenCalledWith({
      action: "accept",
      answers: { name: "Grace", color: "blue", tags: ["x"], qty: 2, agree: true },
    });
  });
});

describe("AskUserQuestionCard decline / cancel", () => {
  it("sends decline on Skip", () => {
    const { onResolve } = renderCard(makeElicitation([makeQuestion({ field_key: "q", kind: "free_text" })]));
    fireEvent.click(screen.getByRole("button", { name: "Skip" }));
    expect(onResolve).toHaveBeenCalledWith({ action: "decline" });
  });

  it("sends cancel on Cancel", () => {
    const { onResolve } = renderCard(makeElicitation([makeQuestion({ field_key: "q", kind: "free_text" })]));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onResolve).toHaveBeenCalledWith({ action: "cancel" });
  });
});

describe("AskUserQuestionCard rollback / offline", () => {
  it("shows a rollback message when onResolve rejects", async () => {
    const onResolve = vi.fn<(r: ElicitationResolution) => Promise<void>>().mockRejectedValue(new Error("boom"));
    render(
      <AskUserQuestionCard
        elicitation={makeElicitation([makeQuestion({ field_key: "q", kind: "free_text", default: "x" })])}
        onResolve={onResolve}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(await screen.findByText("Could not reach the server. Try again.")).toBeTruthy();
  });

  it("disables controls and shows the offline banner when the server is down", () => {
    setServerDown(true);
    const { onResolve } = renderCard(makeElicitation([makeQuestion({ field_key: "q", kind: "free_text" })]));
    expect(screen.getByText("Disconnected — reconnect to use")).toBeTruthy();
    const submit = screen.getByRole("button", { name: "Submit" }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    fireEvent.click(submit);
    expect(onResolve).not.toHaveBeenCalled();
  });
});

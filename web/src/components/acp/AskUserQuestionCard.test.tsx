// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { Elicitation, ElicitationQuestion, ElicitationResolution } from "../../lib/acpTypes";
import { isElicitationAnswersPayload } from "../../lib/acpTypes";
import { setServerDown } from "../../lib/connectionState";
import { AskUserQuestionCard } from "./AskUserQuestionCard";
import { ElicitationAnswerCard } from "./ElicitationAnswerCard";

afterEach(() => {
  setServerDown(false);
  cleanup();
});

function q(overrides: Partial<ElicitationQuestion> & { field_key: string }): ElicitationQuestion {
  return { title: null, description: null, required: false, kind: "free_text", options: [], ...overrides };
}

function renderCard(questions: ElicitationQuestion[], overrides: Partial<Elicitation> = {}) {
  const onResolve = vi.fn<(r: ElicitationResolution) => Promise<void>>().mockResolvedValue(undefined);
  const elicitation: Elicitation = {
    nonce: "nonce-1",
    message: "Please answer the question",
    tool_call_id: null,
    questions,
    requested_at: "2026-01-01T00:00:00Z",
    resolved: null,
    ...overrides,
  };
  render(<AskUserQuestionCard elicitation={elicitation} onResolve={onResolve} />);
  return onResolve;
}

const submit = () => fireEvent.click(screen.getByRole("button", { name: "Submit" }));
const type = (value: string, placeholder = "Type your answer") =>
  fireEvent.change(screen.getByPlaceholderText(placeholder), { target: { value } });
const typeNumber = (value: string) => type(value, "Enter a number");
const check = (name: string) => fireEvent.click(screen.getByRole("checkbox", { name }));
const radio = (name: RegExp) => fireEvent.click(screen.getByRole("radio", { name }));

const COLORS = [
  { value: "red", label: "red" },
  { value: "green", label: "green" },
];
const TAGS = ["a", "b", "c"].map((v) => ({ value: v, label: v }));

/** Each step runs, then Submit; the last step's expectation is an accepted payload or an error. */
type Step = [() => void, Record<string, unknown> | string];

describe("AskUserQuestionCard submission", () => {
  it.each<[string, ElicitationQuestion[], Step[]]>([
    ["free text", [q({ field_key: "name" })], [[() => type("Ada"), { name: "Ada" }]]],
    [
      "required free text",
      [q({ field_key: "req", title: "Required", required: true })],
      [[() => {}, "Please answer: Required"]],
    ],
    [
      "email format",
      [q({ field_key: "mail", title: "Email", format: "email" })],
      [
        [() => type("not-an-email"), "Email is not a valid email"],
        [() => type("a@b.co"), { mail: "a@b.co" }],
      ],
    ],
    [
      "length and pattern",
      [q({ field_key: "code", title: "Code", min_length: 3, max_length: 10, pattern: "^[a-z]+$" })],
      [
        [() => type("ab"), "Code must be at least 3 characters"],
        [() => type("AB12"), "Code does not match the required format"],
        [() => type("abc"), { code: "abc" }],
      ],
    ],
    [
      "max length",
      [q({ field_key: "code", title: "Code", max_length: 3 })],
      [[() => type("toolong"), "Code must be at most 3 characters"]],
    ],
    // The server skips invalid regexes too.
    ["unparseable pattern", [q({ field_key: "any", pattern: "([" })], [[() => type("whatever"), { any: "whatever" }]]],
    [
      "single select",
      [q({ field_key: "color", title: "Pick a color", kind: "single_select", options: COLORS })],
      [[() => radio(/green/), { color: "green" }]],
    ],
    [
      "multi select toggles",
      [q({ field_key: "tags", kind: "multi_select", options: TAGS })],
      [
        [
          () => {
            check("a");
            check("b");
            check("a");
          },
          { tags: ["b"] },
        ],
      ],
    ],
    ["empty optional multi select", [q({ field_key: "tags", kind: "multi_select", options: TAGS })], [[() => {}, {}]]],
    [
      "multi select bounds",
      [
        q({
          field_key: "tags",
          title: "Tags",
          kind: "multi_select",
          required: true,
          min_items: 2,
          max_items: 2,
          options: TAGS,
        }),
      ],
      [
        [() => {}, "Please answer: Tags"],
        [() => check("a"), "Select at least 2 for Tags"],
        [
          () => {
            check("b");
            check("c");
          },
          "Select at most 2 for Tags",
        ],
        [() => check("c"), { tags: ["a", "b"] }],
      ],
    ],
    ["number", [q({ field_key: "qty", kind: "number" })], [[() => typeNumber("3.5"), { qty: 3.5 }]]],
    ["empty optional number", [q({ field_key: "qty", kind: "number" })], [[() => {}, {}]]],
    [
      "required number",
      [q({ field_key: "qty", title: "Qty", kind: "number", required: true })],
      [[() => {}, "Please answer: Qty"]],
    ],
    [
      "number range",
      [q({ field_key: "qty", title: "Qty", kind: "number", minimum: 1, maximum: 5 })],
      [
        [() => typeNumber("0"), "Qty must be at least 1"],
        [() => typeNumber("9"), "Qty must be at most 5"],
        [() => typeNumber("5"), { qty: 5 }],
      ],
    ],
    [
      "integer",
      [q({ field_key: "count", title: "Count", kind: "integer" })],
      [
        [() => typeNumber("2.5"), "Count must be a whole number"],
        [() => typeNumber("4"), { count: 4 }],
      ],
    ],
    // A checkbox always has a definite value.
    [
      "unchecked boolean",
      [q({ field_key: "agree", title: "Agree?", kind: "boolean" })],
      [[() => {}, { agree: false }]],
    ],
    [
      "checked boolean",
      [q({ field_key: "agree", title: "Agree?", kind: "boolean" })],
      [[() => check("Agree?"), { agree: true }]],
    ],
    [
      "every kind in one form",
      [
        q({ field_key: "name", title: "Name" }),
        q({ field_key: "color", title: "Color", kind: "single_select", options: [{ value: "blue", label: "blue" }] }),
        q({ field_key: "tags", title: "Tags", kind: "multi_select", options: [{ value: "x", label: "x" }] }),
        q({ field_key: "qty", title: "Qty", kind: "number" }),
        q({ field_key: "agree", title: "Agree", kind: "boolean" }),
      ],
      [
        [
          () => {
            type("Grace");
            radio(/blue/);
            check("x");
            typeNumber("2");
            check("Agree");
          },
          { name: "Grace", color: "blue", tags: ["x"], qty: 2, agree: true },
        ],
      ],
    ],
  ])("%s", (_label, questions, steps) => {
    const onResolve = renderCard(questions);
    for (const [act, expected] of steps) {
      act();
      submit();
      if (typeof expected === "string") {
        expect(screen.getByText(expected)).toBeTruthy();
        expect(onResolve).not.toHaveBeenCalled();
      } else {
        expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: expected });
      }
    }
  });

  it("seeds defaults across kinds and omits empty optional answers", () => {
    const onResolve = renderCard([
      q({ field_key: "seeded", default: "preset" }),
      q({ field_key: "blank" }),
      q({ field_key: "color", kind: "single_select", options: COLORS, default: "green" }),
      q({ field_key: "tags", kind: "multi_select", options: TAGS, default: ["a", "c"] }),
      q({ field_key: "qty", kind: "number", default: 7 }),
      q({ field_key: "on", title: "On", kind: "boolean", default: true }),
    ]);
    expect((screen.getAllByPlaceholderText("Type your answer")[0] as HTMLInputElement).value).toBe("preset");
    expect((screen.getByRole("radio", { name: "green" }) as HTMLInputElement).checked).toBe(true);
    expect((screen.getByRole("checkbox", { name: "c" }) as HTMLInputElement).checked).toBe(true);
    expect((screen.getByPlaceholderText("Enter a number") as HTMLInputElement).value).toBe("7");
    expect((screen.getByRole("checkbox", { name: "On" }) as HTMLInputElement).checked).toBe(true);
    submit();
    expect(onResolve).toHaveBeenCalledWith({
      action: "accept",
      answers: { seeded: "preset", color: "green", tags: ["a", "c"], qty: 7, on: true },
    });
  });

  it.each([
    ["Skip", "decline"],
    ["Cancel", "cancel"],
  ])("%s resolves with %s", (button, action) => {
    const onResolve = renderCard([q({ field_key: "q" })]);
    fireEvent.click(screen.getByRole("button", { name: button }));
    expect(onResolve).toHaveBeenCalledWith({ action });
  });

  it("shows a rollback message when onResolve rejects", async () => {
    const onResolve = vi.fn<(r: ElicitationResolution) => Promise<void>>().mockRejectedValue(new Error("boom"));
    render(
      <AskUserQuestionCard
        elicitation={{
          nonce: "n",
          message: "m",
          tool_call_id: null,
          questions: [q({ field_key: "q", default: "x" })],
          requested_at: "2026-01-01T00:00:00Z",
          resolved: null,
        }}
        onResolve={onResolve}
      />,
    );
    submit();
    expect(await screen.findByText("Could not reach the server. Try again.")).toBeTruthy();
  });

  it("disables controls and shows the offline banner when the server is down", () => {
    setServerDown(true);
    const onResolve = renderCard([q({ field_key: "q" })]);
    expect(screen.getByText("Disconnected — reconnect to use")).toBeTruthy();
    const button = screen.getByRole("button", { name: "Submit" }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    fireEvent.click(button);
    expect(onResolve).not.toHaveBeenCalled();
  });
});

describe("AskUserQuestionCard rendering", () => {
  it("renders the dialog chrome, schema title and description, and the full unwrapped prompt", () => {
    const message =
      "This is a deliberately long question that should wrap onto multiple lines rather than being clipped.";
    renderCard([q({ field_key: "c", kind: "single_select", options: COLORS })], {
      message,
      title: "Your profile",
      description: "These help tailor the result.",
    });
    expect(screen.getByRole("alertdialog", { name: /Question from the agent/i })).toBeTruthy();
    expect(screen.getAllByRole("radio")).toHaveLength(2);
    expect(screen.getByText("Your profile")).toBeTruthy();
    expect(screen.getByText("These help tailor the result.")).toBeTruthy();
    const prompt = screen.getByText(message);
    expect(prompt.className).toContain("whitespace-pre-wrap");
    expect(prompt.className).not.toContain("truncate");
  });

  it("renders a plain free-text field as a multiline textarea", () => {
    renderCard([q({ field_key: "name" })]);
    expect(screen.getByPlaceholderText("Type your answer").tagName).toBe("TEXTAREA");
  });

  it("inserts a newline on Shift+Enter in the free-text textarea instead of submitting", () => {
    const onResolve = renderCard([q({ field_key: "name" })]);
    const textarea = screen.getByPlaceholderText("Type your answer");
    expect(fireEvent.keyDown(textarea, { key: "Enter", shiftKey: true })).toBe(true);
    expect(onResolve).not.toHaveBeenCalled();
  });

  it("does not submit the free-text textarea on Enter during IME composition", () => {
    const onResolve = renderCard([q({ field_key: "name" })]);
    const textarea = screen.getByPlaceholderText("Type your answer");
    fireEvent.change(textarea, { target: { value: "Ada" } });
    expect(fireEvent.keyDown(textarea, { key: "Enter", isComposing: true })).toBe(true);
    expect(onResolve).not.toHaveBeenCalled();
  });

  it("submits the free-text textarea on Enter without Shift", () => {
    const onResolve = renderCard([q({ field_key: "name" })]);
    const textarea = screen.getByPlaceholderText("Type your answer");
    fireEvent.change(textarea, { target: { value: "Ada" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { name: "Ada" } });
  });

  // Older adapters flattened `"<label> — <description>"` into the title; a structured
  // description (even empty) wins, and null falls back to the split.
  it.each([
    ["flattened label", { label: "Red — the warm one" }, "the warm one", null],
    ["structured description", { label: "Red", description: "the warm one" }, "the warm one", null],
    ["structured over flattened", { label: "Red — stale", description: "the warm one" }, "the warm one", "stale"],
    ["empty structured description", { label: "Red — the warm one", description: "" }, null, "the warm one"],
    ["null structured description", { label: "Red — the warm one", description: null }, "the warm one", null],
  ])("splits option text for a %s", (_label, option, shown, hidden) => {
    const onResolve = renderCard([
      q({ field_key: "question_0", kind: "single_select", title: "Pick", options: [{ value: "Red", ...option }] }),
    ]);
    expect(screen.getByText("Red")).toBeTruthy();
    if (shown) expect(screen.getByText(shown)).toBeTruthy();
    if (hidden) expect(screen.queryByText(hidden)).toBeNull();
    fireEvent.click(screen.getAllByRole("radio")[0]!);
    submit();
    expect(onResolve).toHaveBeenCalledWith({ action: "accept", answers: { question_0: "Red" } });
  });
});

describe("ElicitationAnswerCard", () => {
  it("renders each pair with a count label", () => {
    const answers = [
      { question: "Color?", answer: "Blue" },
      { question: "Languages?", answer: "Rust, TypeScript" },
    ];
    render(<ElicitationAnswerCard answers={answers} />);
    expect(screen.getByText("2 answers")).toBeTruthy();
    for (const a of answers) {
      expect(screen.getByText(a.question)).toBeTruthy();
      expect(screen.getByText(a.answer)).toBeTruthy();
    }
  });

  it.each([
    [[{ question: "q", answer: "a" }], true],
    [[], false],
    [undefined, false],
    ["nope", false],
    [[{ question: "q" }], false],
    [[{ question: 1, answer: 2 }], false],
  ])("isElicitationAnswersPayload(%j) = %s", (value, expected) => {
    expect(isElicitationAnswersPayload(value)).toBe(expected);
  });
});

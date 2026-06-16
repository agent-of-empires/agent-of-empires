// @vitest-environment jsdom
//
// Render tests for ElicitationAnswerCard, the rich AskUserQuestion / elicitation
// answer card in the structured-view user-message slot (#2209). Covers the
// answer count label, each question/answer pair, and the payload type guard
// that gates the card vs the plain-text fallback in UserText.

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { ElicitationAnswerCard } from "./ElicitationAnswerCard";
import { isElicitationAnswersPayload } from "../../lib/acpTypes";

afterEach(() => cleanup());

describe("ElicitationAnswerCard", () => {
  it("renders each question/answer pair and a count label", () => {
    render(
      <ElicitationAnswerCard
        answers={[
          { question: "Color?", answer: "Blue" },
          { question: "Languages?", answer: "Rust, TypeScript" },
        ]}
      />,
    );
    expect(screen.getByText("2 answers")).toBeTruthy();
    expect(screen.getByText("Color?")).toBeTruthy();
    expect(screen.getByText("Blue")).toBeTruthy();
    expect(screen.getByText("Languages?")).toBeTruthy();
    expect(screen.getByText("Rust, TypeScript")).toBeTruthy();
  });

  it("uses the singular label for a single answer", () => {
    render(<ElicitationAnswerCard answers={[{ question: "Proceed?", answer: "Yes" }]} />);
    expect(screen.getByText("1 answer")).toBeTruthy();
  });
});

describe("isElicitationAnswersPayload", () => {
  it("accepts a non-empty array of question/answer pairs", () => {
    expect(isElicitationAnswersPayload([{ question: "q", answer: "a" }])).toBe(true);
  });

  it("rejects an empty array, non-arrays, and malformed entries", () => {
    expect(isElicitationAnswersPayload([])).toBe(false);
    expect(isElicitationAnswersPayload(undefined)).toBe(false);
    expect(isElicitationAnswersPayload("nope")).toBe(false);
    expect(isElicitationAnswersPayload([{ question: "q" }])).toBe(false);
    expect(isElicitationAnswersPayload([{ question: 1, answer: 2 }])).toBe(false);
  });
});

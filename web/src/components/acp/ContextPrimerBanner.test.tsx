// @vitest-environment jsdom
//
// Context-primer banner. When the structured view detects a context-reset
// (`session/load` failure with prior turns in SQLite), this banner
// offers the user a recap fetched from
// `GET /api/sessions/:id/acp/context-primer`. The component owns
// the loading/error transients and the Insert vs Dismiss routing.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

vi.mock("../../lib/api", () => ({
  fetchContextPrimer: vi.fn(),
}));

import { ContextPrimerBanner } from "./ContextPrimerBanner";
import { fetchContextPrimer } from "../../lib/api";

const mockFetch = vi.mocked(fetchContextPrimer);

const AVAILABLE = { resetSeq: 7, reason: "session/load" };
const primer = (text: string) => ({
  primer: text,
  included_event_count: text ? 4 : 0,
  included_turn_count: text ? 2 : 0,
  truncated: false,
  max_chars: 4000,
  unprocessed_prompt: null,
});

function mount(props?: Partial<React.ComponentProps<typeof ContextPrimerBanner>>) {
  const onInsertPrimer = vi.fn();
  const onDismiss = vi.fn();
  const utils = render(
    <ContextPrimerBanner
      sessionId="s-1"
      available={AVAILABLE}
      onInsertPrimer={onInsertPrimer}
      onDismiss={onDismiss}
      {...props}
    />,
  );
  return { onInsertPrimer, onDismiss, ...utils };
}

beforeEach(() => {
  mockFetch.mockReset();
});

afterEach(() => {
  cleanup();
});

describe("ContextPrimerBanner", () => {
  it("calls onDismiss when the × button is clicked", () => {
    const { getByLabelText, onDismiss } = mount();
    fireEvent.click(getByLabelText("Dismiss context-reset banner"));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  it("fetches the primer for the session and reset seq, inserts it, and dismisses", async () => {
    mockFetch.mockResolvedValueOnce(primer("recap text"));
    const { getByText, onInsertPrimer, onDismiss } = mount();
    fireEvent.click(getByText(/Resume with prior context/i));
    await waitFor(() => expect(onInsertPrimer).toHaveBeenCalledWith("recap text"));
    expect(mockFetch).toHaveBeenCalledExactlyOnceWith("s-1", 7, expect.anything());
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  it.each([
    ["a null response", () => mockFetch.mockResolvedValueOnce(null), /Failed to fetch primer/i],
    ["an empty primer", () => mockFetch.mockResolvedValueOnce(primer("")), /No prior transcript/i],
    ["a rejection", () => mockFetch.mockRejectedValueOnce(new Error("network down")), /Network error/i],
  ])("surfaces an error for %s without inserting or dismissing", async (_label, arrange, message) => {
    arrange();
    const { getByText, findByRole, onInsertPrimer, onDismiss } = mount();
    fireEvent.click(getByText(/Resume with prior context/i));
    expect((await findByRole("alert")).textContent).toMatch(message);
    expect(onInsertPrimer).not.toHaveBeenCalled();
    expect(onDismiss).not.toHaveBeenCalled();
  });

  it("ignores an AbortError without surfacing an error message", async () => {
    const err = Object.assign(new Error("aborted"), { name: "AbortError" });
    mockFetch.mockRejectedValueOnce(err);
    const { getByText, queryByRole, onInsertPrimer } = mount();
    fireEvent.click(getByText(/Resume with prior context/i));
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());
    // No alert role surfaces from an AbortError.
    await Promise.resolve();
    expect(queryByRole("alert")?.textContent ?? "").not.toMatch(/Network error/i);
    expect(onInsertPrimer).not.toHaveBeenCalled();
  });

  it("clears prior error state when resetSeq changes", async () => {
    mockFetch.mockResolvedValueOnce(null);
    const { getByText, findByRole, rerender, queryByRole } = render(
      <ContextPrimerBanner sessionId="s-1" available={AVAILABLE} onInsertPrimer={vi.fn()} onDismiss={vi.fn()} />,
    );
    fireEvent.click(getByText(/Resume with prior context/i));
    await findByRole("alert");
    rerender(
      <ContextPrimerBanner
        sessionId="s-1"
        available={{ resetSeq: 8, reason: "new reset" }}
        onInsertPrimer={vi.fn()}
        onDismiss={vi.fn()}
      />,
    );
    expect(queryByRole("alert")).toBeNull();
  });
});

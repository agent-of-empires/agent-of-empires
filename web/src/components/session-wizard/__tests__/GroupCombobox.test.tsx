// @vitest-environment jsdom

import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import type { GroupInfo } from "../../../lib/types";
import { GroupCombobox } from "../steps/GroupCombobox";
import { groupOptions } from "../steps/groupOptions";

const GROUPS: GroupInfo[] = [
  { path: "client", session_count: 0 },
  { path: "client/api", session_count: 2 },
  { path: "client/web", session_count: 1 },
  { path: "old", session_count: 3, archived: true },
  { path: "studio", session_count: 2 },
];

afterEach(cleanup);

describe("groupOptions", () => {
  const paths = (text: string, groups = GROUPS) =>
    groupOptions(groups, text).options.map((o) => (o.kind === "group" ? o.group.path : `+${o.path}`));
  const highlighted = (text: string, groups = GROUPS) => {
    const { options, highlight } = groupOptions(groups, text);
    const o = options[highlight];
    return o ? (o.kind === "group" ? o.group.path : `+${o.path}`) : null;
  };

  it.each<[string, string, string[], string | null]>([
    [
      "an empty field lists every group, archived last, with no create row",
      "",
      ["client", "client/api", "client/web", "studio", "old"],
      null,
    ],
    ["a substring filters on the full path", "client/", ["client/api", "client/web", "+client/"], "+client/"],
    [
      "an exact match is highlighted and offers no create row",
      "client",
      ["client", "client/api", "client/web"],
      "client",
    ],
    ["a new name gets the create row, highlighted", "Stu", ["studio", "+Stu"], "+Stu"],
    ["matching ignores case", "STUDIO", ["studio", "+STUDIO"], "+STUDIO"],
    ["surrounding spaces don't count", "  studio ", ["studio"], "studio"],
  ])("%s", (_label, text, expected, highlight) => {
    expect(paths(text)).toEqual(expected);
    expect(highlighted(text)).toBe(highlight);
  });

  it("with no groups at all, a name offers only the create row", () => {
    expect(paths("", [])).toEqual([]);
    expect(paths("new", [])).toEqual(["+new"]);
  });
});

function Harness({ groups = GROUPS, initial = "" }: { groups?: GroupInfo[]; initial?: string }) {
  const [value, setValue] = useState(initial);
  return (
    <>
      <GroupCombobox id="g" value={value} onChange={setValue} groups={groups} />
      <output data-testid="value">{value}</output>
    </>
  );
}

const input = () => screen.getByRole("combobox") as HTMLInputElement;
const value = () => screen.getByTestId("value").textContent;
const type = (text: string) => fireEvent.change(input(), { target: { value: text } });
const key = (k: string) => fireEvent.keyDown(input(), { key: k });
const activeOption = () => {
  const id = input().getAttribute("aria-activedescendant");
  return id ? document.getElementById(id)?.getAttribute("aria-label") : null;
};

describe("GroupCombobox", () => {
  it("is a combobox wired to its listbox, with counts in each option's name", () => {
    render(<Harness />);
    expect(input().getAttribute("aria-expanded")).toBe("false");
    fireEvent.focus(input());
    expect(input().getAttribute("aria-expanded")).toBe("true");
    const list = screen.getByRole("listbox");
    expect(input().getAttribute("aria-controls")).toBe(list.id);
    expect(screen.getByRole("option", { name: "client/api · 2 sessions" })).toBeTruthy();
    expect(screen.getByRole("option", { name: "client/web · 1 session" })).toBeTruthy();
    expect(screen.getByRole("option", { name: "old · 3 sessions · archived" })).toBeTruthy();
    expect(screen.getByTestId("group-combobox-popup").textContent).toContain(
      "A named heading in the sidebar for sessions you want together, across Projects. Use / to nest, e.g. client/api.",
    );
  });

  it("Enter on a new name keeps the typed text, never a longer match", () => {
    render(<Harness />);
    fireEvent.focus(input());
    type("Stu");
    expect(activeOption()).toBe('Create group "Stu"');
    key("Enter");
    expect(value()).toBe("Stu");
    expect(input().getAttribute("aria-expanded")).toBe("false");
  });

  it("arrows move the highlight and Enter picks it", () => {
    render(<Harness />);
    fireEvent.focus(input());
    type("client/");
    key("ArrowUp");
    expect(activeOption()).toBe("client/web · 1 session");
    key("ArrowUp");
    expect(activeOption()).toBe("client/api · 2 sessions");
    key("Enter");
    expect(value()).toBe("client/api");
  });

  // The wizard renders before `/api/groups` answers, so groups can arrive after typing.
  it.each([
    ["a new name", "Stu", 'Create group "Stu"', "Stu"],
    ["a name that turns out to exist", "studio", "studio · 2 sessions", "studio"],
  ])("groups arriving after typing %s keep Enter on the right row", (_label, typed, highlighted, picked) => {
    const { rerender } = render(<Harness groups={[]} />);
    fireEvent.focus(input());
    type(typed);
    rerender(<Harness groups={GROUPS} />);
    expect(activeOption()).toBe(highlighted);
    key("Enter");
    expect(value()).toBe(picked);
  });

  it("scrolls the highlighted row into view", () => {
    const scrolled: (string | null)[] = [];
    const spy = vi.spyOn(Element.prototype, "scrollIntoView").mockImplementation(function (this: Element) {
      scrolled.push(this.getAttribute("aria-label"));
    });
    try {
      render(<Harness />);
      fireEvent.focus(input());
      type("client/");
      key("ArrowUp");
      expect(scrolled.at(-1)).toBe("client/web · 1 session");
    } finally {
      spy.mockRestore();
    }
  });

  it("a click picks a group", () => {
    render(<Harness />);
    fireEvent.focus(input());
    fireEvent.mouseDown(screen.getByRole("option", { name: "studio · 2 sessions" }));
    expect(value()).toBe("studio");
    expect(input().getAttribute("aria-expanded")).toBe("false");
  });

  it.each([
    ["Esc", () => key("Escape")],
    ["blur", () => fireEvent.blur(input())],
  ])("%s closes the list and keeps the text", (_label, close) => {
    render(<Harness />);
    fireEvent.focus(input());
    type("cli");
    close();
    expect(input().getAttribute("aria-expanded")).toBe("false");
    expect(value()).toBe("cli");
  });

  it("with no groups, an empty field shows only the hint", () => {
    render(<Harness groups={[]} />);
    fireEvent.focus(input());
    expect(screen.queryAllByRole("option")).toHaveLength(0);
    expect(screen.getByTestId("group-combobox-popup").textContent).toBe("No groups yet. Type a name to create one.");
  });
});

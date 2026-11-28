// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";

import { NewGroupDialog } from "../sidebar/NewGroupDialog";
import { makeSession } from "./fixtures";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const SESSIONS = [
  makeSession({ id: "a", title: "alpha", profile: "default", group_path: "" }),
  makeSession({ id: "b", title: "bravo", profile: "default", group_path: "" }),
  makeSession({ id: "w", title: "work-only", profile: "work", group_path: "" }),
];

function setup(respond: () => Response) {
  const spy = vi.fn<typeof fetch>(async () => respond());
  vi.stubGlobal("fetch", spy);
  const onCreated = vi.fn();
  render(<NewGroupDialog sessions={SESSIONS} selectedId="b" onCancel={vi.fn()} onCreated={onCreated} />);
  return { spy, onCreated };
}

const boxes = () =>
  within(screen.getByTestId("new-group-sessions"))
    .getAllByRole("checkbox")
    .map((b) => [b.closest("label")!.textContent, (b as HTMLInputElement).checked]);

describe("NewGroupDialog", () => {
  it("lists a profile's Sessions once one becomes movable after it opened", () => {
    const props = { selectedId: null, onCancel: vi.fn(), onCreated: vi.fn() };
    const { rerender } = render(
      <NewGroupDialog
        sessions={[makeSession({ id: "a", title: "alpha", group_path: "", status: "Creating" })]}
        {...props}
      />,
    );
    expect(within(screen.getByTestId("new-group-sessions")).queryAllByRole("checkbox")).toEqual([]);
    rerender(<NewGroupDialog sessions={[makeSession({ id: "a", title: "alpha", group_path: "" })]} {...props} />);
    expect(boxes()).toEqual([["alpha", false]]);
  });

  it("keeps Tab inside and hands focus back to its opener on close", () => {
    const opener = document.createElement("button");
    document.body.append(opener);
    opener.focus();
    const { unmount } = render(
      <NewGroupDialog sessions={SESSIONS} selectedId="b" onCancel={vi.fn()} onCreated={vi.fn()} />,
    );
    const create = screen.getByTestId("new-group-create");
    create.focus();
    fireEvent.keyDown(create, { key: "Tab" });
    expect(document.activeElement).toBe(screen.getByLabelText("Group name"));
    unmount();
    expect(document.activeElement).toBe(opener);
    opener.remove();
  });
  it.each<[string, boolean, string[]]>([
    ["offers only Sessions the Daemon can move", false, ["alpha", "terminal"]],
    ["offers only Structured view Sessions under CityHall", true, ["alpha"]],
  ])("%s", (_, structuredOnly, offered) => {
    const sessions = [
      makeSession({ id: "a", title: "alpha", group_path: "", view: "structured" }),
      makeSession({ id: "t", title: "terminal", group_path: "", view: "terminal" }),
      makeSession({ id: "c", title: "creating", group_path: "", view: "structured", status: "Creating" }),
      makeSession({ id: "d", title: "deleting", group_path: "", view: "structured", status: "Deleting" }),
    ];
    render(
      <NewGroupDialog
        sessions={sessions}
        selectedId="t"
        structuredOnly={structuredOnly}
        onCancel={vi.fn()}
        onCreated={vi.fn()}
      />,
    );
    expect(boxes().map(([label]) => label)).toEqual(offered);
  });

  it("starts with the selected Session checked, in its profile, and creates the Group with the picks", async () => {
    const { spy, onCreated } = setup(() => new Response(JSON.stringify({ ok: true }), { status: 201 }));
    expect(boxes()).toEqual([
      ["alpha", false],
      ["bravo", true],
    ]);
    fireEvent.change(screen.getByLabelText("Group name"), { target: { value: " work/api/ " } });
    fireEvent.click(screen.getByRole("checkbox", { name: "alpha" }));
    fireEvent.click(screen.getByTestId("new-group-create"));
    await waitFor(() => expect(onCreated).toHaveBeenCalledTimes(1));
    const [url, init] = spy.mock.calls[0]!;
    expect([url, init?.method, JSON.parse(init!.body as string)]).toEqual([
      "/api/groups",
      "POST",
      { profile: "default", path: "work/api", sessions: ["a", "b"] },
    ]);
  });

  it("never sends a Group without a Session, and names the Daemon's refusal", async () => {
    const { spy, onCreated } = setup(() => new Response(JSON.stringify({ error: "conflict" }), { status: 409 }));
    fireEvent.change(screen.getByLabelText("Group name"), { target: { value: "ops" } });
    fireEvent.click(screen.getByRole("checkbox", { name: "bravo" }));
    fireEvent.click(screen.getByTestId("new-group-create"));
    expect(screen.getByRole("alert").textContent).toBe("Pick at least one session.");
    expect(spy).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("checkbox", { name: "alpha" }));
    fireEvent.click(screen.getByTestId("new-group-create"));
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe("That group exists already, or a picked session is busy."),
    );
    expect(onCreated).not.toHaveBeenCalled();
  });

  it("lists one profile at a time, since the Daemon moves one profile's Sessions per Group", () => {
    setup(() => new Response(null, { status: 500 }));
    fireEvent.change(screen.getByTestId("new-group-profile"), { target: { value: "work" } });
    expect(boxes()).toEqual([["work-only", false]]);
  });
});

---
name: aoe-keyboard-focus
description: Review an Agent of Empires web dashboard or terminal UI change for whether every new control is reachable and usable from the keyboard, whether focus lands, stays and returns where the user expects as views open, load, retry and close, and whether content stays visible at narrow widths. Use as one lens in a code review run.
---

# Review lens: AoE keyboard and focus

The web dashboard and the terminal UI are keyboard-driven (DESIGN.md "Web dashboard"). Review what a change does to the keyboard path and to focus.

## Scope

- **Reachability** Whether each action the change adds can be reached and triggered from the keyboard, not only by mouse, hover or right-click, and whether scrollable or interactive content is focusable and has an accessible name.
- **Focus placement** Whether focus moves only on the user's intent: autofocus or a focus effect that fires again after loading, a retry, a remount or a status change can take focus from where the user put it. Whether focus returns to the opener, or to a sensible neighbour, when a dialog, menu or strip closes or hides the focused control.
- **Containment** Whether a modal keeps Tab and Shift+Tab inside it in every state it can be in, including disabled, pending and loading states, and whether Escape closes only the topmost layer. A radio group is one tab stop, the checked radio, so a modal's first and last stops depend on which radio is checked.
- **Narrow widths** Whether new text, hints, footers and controls stay visible or scrollable at the narrowest layout the surface supports (a narrow terminal, a phone viewport), and whether click targets still match what is drawn after a resize or refresh.

## Method

Start from each control, dialog, menu or row the diff adds or changes. Walk it from the keyboard: how focus arrives, what Tab, Shift+Tab, Enter, Space, the arrow keys and Escape do at each step, and where focus goes when it closes. For each focus call or `autoFocus`, list the renders that can run it again (data arriving, a retry, a status change, a remount) and check what the user may have focused by then. For a modal, enumerate its states from the code and check containment in each. For new text in a fixed-width area, find the narrowest layout the surface renders and check what clips.

## Threshold

Report when a keyboard user cannot reach or complete an action the change adds, when focus is taken from the user or lost to the page body, when Tab escapes a modal, or when new content is cut off with no way to see it.

Do not report:
- Which handler a key or paste reaches while a view opens, closes or changes selection, and key repeats crossing into the next action; `aoe-runtime-lifecycle` owns them.
- Test code, and whether a test runs focus or layout behavior in a browser rather than jsdom; `aoe-test-evidence` owns them.
- Hiding or disabling controls in read-only mode; `review-security-and-privacy` owns it.
- Colour, contrast, spacing and other visual styling, and copy wording.
- Behavior the change does not touch, and pre-existing gaps it only moves.

## Reporting

- Name the control or view, the key sequence or render that goes wrong, and what the user ends up with.
- State the fix: the keyboard binding or focusable element to add, the user-intent signal to gate autofocus on, the focus target to restore, the containment rule for the state that leaks, or the wrap or scroll to add.

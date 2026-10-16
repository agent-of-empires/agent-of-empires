# Status detection fixtures

Terminal captures are grouped by tool and expected state:

```text
tests/fixtures/<tool>/<idle|running|waiting_permission|waiting_question>/
```

Capture the current pane of an AoE-managed tmux session with:

```sh
./scripts/capture-fixtures.sh <tool> <state> <tmux-session> [description]
```

The script creates a numbered `NNN_description.txt` file with metadata. Review
the capture, remove unrelated or sensitive terminal content, then run the
status-detection tests with `cargo test --test integration status_detection`.

Put regression captures in the state the tool should have detected. Multiple
captures per state cover UI variations without adding separate test functions.

For OMP approval captures, use an isolated session with explicit approval enabled.
Capture both collapsed and expanded details (`Ctrl+O`), including an unsent
steering draft that wraps in the composer. Do not approve or submit the draft
before capturing. Use distinct descriptions for each layout.

Set `AOE_TMUX_SOCKET` to capture from an isolated tmux server, and
`AOE_FIXTURES_DIR` to write into an alternate fixture directory.

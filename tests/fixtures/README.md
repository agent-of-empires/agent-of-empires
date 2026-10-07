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
status-detection tests with `cargo test status_detection`.

Put regression captures in the state the tool should have detected. Multiple
captures per state cover UI variations without adding separate test functions.

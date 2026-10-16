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

For OMP's native approval prompt, generate a temporary approval overlay before
starting an AoE-managed scratch session:

```sh
overlay_dir="$(mktemp -d "${TMPDIR:-/tmp}/omp-fixture.XXXXXX")"
custom_overlay="$overlay_dir/custom.yml"
./scripts/capture-fixtures.sh --write-omp-approval-overlay "$custom_overlay" custom
aoe add --scratch --title omp-fixture-custom --tool omp \
  --extra-args "--config '$custom_overlay' --approval-mode always-ask"
aoe session start omp-fixture-custom
```

The overlay sets `tools.approvalMode: always-ask` and
`tools.approval.bash: prompt`, which overrides an existing per-tool `allow`
without changing persistent OMP settings. It also sets
`statusLine.preset: custom`; the custom segments remain those from the existing
OMP config. Omit the preset argument to preserve all existing status-line
settings.

OMP supports `default`, `minimal`, `compact`, `full`, `nerd`, `ascii`, and
`custom` status-line presets. To capture a second variant, use a separate
overlay and scratch session:

```sh
compact_overlay="$overlay_dir/compact.yml"
./scripts/capture-fixtures.sh --write-omp-approval-overlay "$compact_overlay" compact
aoe add --scratch --title omp-fixture-compact --tool omp \
  --extra-args "--config '$compact_overlay' --approval-mode always-ask"
aoe session start omp-fixture-compact
```

Ask OMP in each session to run a harmless shell command such as `pwd`, then
capture while the approval prompt is visible. To capture expanded details,
ask OMP for a harmless command with a long argument, press `Ctrl+O` in the
pane, then capture the expanded approval:

```sh
./scripts/capture-fixtures.sh omp waiting_permission <custom-tmux-session> tool_approval_custom
./scripts/capture-fixtures.sh omp waiting_permission <compact-tmux-session> tool_approval_compact
./scripts/capture-fixtures.sh omp waiting_permission <expanded-tmux-session> tool_approval_expanded
rm "$custom_overlay" "$compact_overlay"
rmdir "$overlay_dir"
```

Use a distinct description for each status-line or pane-layout variant so each
capture is checked by the same status-detection test. Review each capture for
sensitive terminal content before committing it.

Set `AOE_TMUX_SOCKET` to capture from an isolated tmux server, and
`AOE_FIXTURES_DIR` to write into an alternate fixture directory during review
or testing.

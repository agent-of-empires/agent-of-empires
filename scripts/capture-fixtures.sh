#!/bin/bash
# Capture terminal fixtures for status detection golden tests
#
# Usage:
#   ./scripts/capture-fixtures.sh <tool> <state> <tmux_session> [description]
#
# Examples:
#   ./scripts/capture-fixtures.sh claude running aoe_myproject_abc12345
#   ./scripts/capture-fixtures.sh claude running aoe_myproject_abc12345 "tool_call"
#   ./scripts/capture-fixtures.sh opencode waiting_question aoe_task_def67890 "clarification"
#
# States: running, waiting_question, waiting_permission, idle
#
# This script captures the current terminal content from a tmux session
# and saves it as a fixture file for golden testing. Each state is a directory
# containing one or more fixture files, allowing multiple examples per state.

set -e

capitalize() {
    echo "$1" | awk '{print toupper(substr($0,1,1)) tolower(substr($0,2))}'
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
FIXTURES_DIR="$PROJECT_ROOT/tests/fixtures"

usage() {
    echo "Usage: $0 <tool> <state> <tmux_session> [description]"
    echo ""
    echo "Arguments:"
    echo "  tool          Tool name: 'claude' or 'opencode'"
    echo "  state         State to capture: 'running', 'waiting_question', 'waiting_permission', 'idle'"
    echo "  tmux_session  Name of the tmux session to capture from"
    echo "  description   Optional description for the fixture filename (e.g., 'tool_call')"
    echo ""
    echo "Examples:"
    echo "  $0 claude running aoe_myproject_abc12345"
    echo "  $0 claude running aoe_myproject_abc12345 tool_call"
    echo "  $0 opencode waiting_question aoe_task_def67890 clarification"
    echo ""
    echo "Output filename format: NNN_description.txt (e.g., 001_capture.txt, 002_tool_call.txt)"
    echo ""
    echo "Steps to capture a fixture:"
    echo "  1. Start the tool in a tmux session managed by aoe"
    echo "  2. Get the tool into the desired state (running, waiting, etc.)"
    echo "  3. Run this script with the appropriate arguments"
    echo "  4. Verify the captured output looks correct"
    echo "  5. Run 'cargo test --test integration status_detection' to verify detection works"
    exit 1
}

# Validate arguments
if [ $# -lt 3 ] || [ $# -gt 4 ]; then
    usage
fi

TOOL="$1"
STATE="$2"
SESSION="$3"
DESCRIPTION="${4:-capture}"

# Sanitize description (replace spaces/special chars with underscores)
DESCRIPTION=$(echo "$DESCRIPTION" | tr ' ' '_' | tr -cd '[:alnum:]_')

# Validate tool
case "$TOOL" in
    claude|claude_code)
        TOOL_DIR="claude_code"
        TOOL_DISPLAY="Claude Code"
        ;;
    opencode)
        TOOL_DIR="opencode"
        TOOL_DISPLAY="OpenCode"
        ;;
    *)
        echo "Error: Invalid tool '$TOOL'. Must be 'claude' or 'opencode'."
        exit 1
        ;;
esac

# Validate state
case "$STATE" in
    running|waiting_question|waiting_permission|idle)
        ;;
    *)
        echo "Error: Invalid state '$STATE'."
        echo "Valid states: running, waiting_question, waiting_permission, idle"
        exit 1
        ;;
esac

# Check if tmux session exists
if ! tmux has-session -t "$SESSION" 2>/dev/null; then
    echo "Error: tmux session '$SESSION' does not exist."
    echo ""
    echo "Available sessions:"
    tmux list-sessions 2>/dev/null || echo "  (no sessions)"
    exit 1
fi

# Create state directory if needed
OUTPUT_DIR="$FIXTURES_DIR/$TOOL_DIR/$STATE"
mkdir -p "$OUTPUT_DIR"

# Find next sequence number
NEXT_NUM=1
if [ -d "$OUTPUT_DIR" ]; then
    EXISTING=$(ls "$OUTPUT_DIR"/*.txt 2>/dev/null | wc -l | tr -d ' ')
    if [ "$EXISTING" -gt 0 ]; then
        LAST_NUM=$(ls "$OUTPUT_DIR"/*.txt 2>/dev/null | sed 's/.*\/\([0-9]*\)_.*/\1/' | sort -n | tail -1)
        NEXT_NUM=$((10#$LAST_NUM + 1))
    fi
fi

# Format sequence number with zero-padding
SEQ_NUM=$(printf "%03d" "$NEXT_NUM")
OUTPUT_FILE="$OUTPUT_DIR/${SEQ_NUM}_${DESCRIPTION}.txt"

# Get tool version if possible
get_version() {
    case "$TOOL_DIR" in
        claude_code)
            claude --version 2>/dev/null | head -1 || echo "unknown"
            ;;
        opencode)
            opencode --version 2>/dev/null | head -1 || echo "unknown"
            ;;
    esac
}

VERSION=$(get_version)
VERSION="${VERSION:-unknown}"
DATE=$(date +%Y-%m-%d)

# Coarse "1.17.x"-style range from the captured version, so reviewers can
# spot fixture drift at a glance. Returns the input unchanged on unknown shapes.
verified_range() {
    if [[ "${1#v}" =~ ^([0-9]+)\.([0-9]+) ]]; then
        echo "${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.x"
    else
        echo "$1"
    fi
}
VERIFIED_AGAINST=$(verified_range "$VERSION")

# Capture pane content (last 50 lines to match detection logic)
CONTENT=$(tmux capture-pane -t "$SESSION" -p -S -50)

# Capitalize state for display
STATE_DISPLAY=$(capitalize "$STATE")

# Write fixture file with header
cat > "$OUTPUT_FILE" << EOF
# FIXTURE: $TOOL_DISPLAY - $STATE_DISPLAY State
# Captured from: $VERSION
# Capture date: $DATE
# Verified against: $VERIFIED_AGAINST (last check: $DATE)
# To add more: scripts/capture-fixtures.sh $TOOL $STATE <tmux_session> [description]
#
# Expected status: $(echo "$STATE" | sed 's/waiting.*/Waiting/; s/running/Running/; s/idle/Idle/')
# Key indicators: (update this after reviewing the capture)

$CONTENT
EOF

echo "Fixture captured successfully!"
echo ""
echo "Output: $OUTPUT_FILE"
echo ""
echo "Next steps:"
echo "  1. Review the captured content: cat $OUTPUT_FILE"
echo "  2. Update the 'Key indicators' comment if needed"
echo "  3. Run tests: cargo test --test integration status_detection"
echo ""
echo "If tests fail, you may need to update the detection logic in:"
echo "  src/tmux/session.rs (detect_claude_status or detect_opencode_status)"

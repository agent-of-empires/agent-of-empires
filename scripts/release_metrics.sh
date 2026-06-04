#!/bin/bash
#
# GitHub Release Download Metrics Report
# Analyzes download statistics for agent-of-empires/agent-of-empires releases
#

REPO="agent-of-empires/agent-of-empires"
API_URL="https://api.github.com/repos/${REPO}/releases"

echo "Fetching release data from GitHub..."

# Fetch all releases (up to 100)
RELEASES=$(curl -s -H "Accept: application/vnd.github.v3+json" \
    -H "User-Agent: release-metrics-script" \
    "${API_URL}?per_page=100")

if [ -z "$RELEASES" ] || [ "$RELEASES" = "[]" ]; then
    echo "No releases found or error fetching data."
    exit 1
fi

# Check for API errors
if echo "$RELEASES" | jq -e '.message' >/dev/null 2>&1; then
    echo "GitHub API error: $(echo "$RELEASES" | jq -r '.message')"
    exit 1
fi

# Create temp files
TEMP_DIR=$(mktemp -d)
VERSIONS_FILE="$TEMP_DIR/versions.txt"
ASSETS_FILE="$TEMP_DIR/assets.txt"

# Extract version info using jq
echo "$RELEASES" | jq -r '.[] | "\(.tag_name)|\(.published_at | split("T")[0])"' > "$VERSIONS_FILE"

# Extract all assets with download counts (excluding .sha256 checksum files)
# Format: download_count|asset_name|version
echo "$RELEASES" | jq -r '
    .[] |
    .tag_name as $ver |
    .assets[] |
    select(.name | test("\\.(sha256|sha512|md5|asc|sig)$") | not) |
    "\(.download_count)|\(.name)|\($ver)"
' > "$ASSETS_FILE"

# Calculate totals
TOTAL_RELEASES=$(wc -l < "$VERSIONS_FILE" | tr -d ' ')
TOTAL_RELEASES=${TOTAL_RELEASES:-0}

TOTAL_DOWNLOADS=$(cut -d'|' -f1 "$ASSETS_FILE" 2>/dev/null | awk '{sum+=$1} END {print sum+0}')
TOTAL_DOWNLOADS=${TOTAL_DOWNLOADS:-0}

# Add download totals to versions
> "$VERSIONS_FILE.final"
while IFS='|' read -r ver date; do
    total=$(grep "|${ver}$" "$ASSETS_FILE" 2>/dev/null | cut -d'|' -f1 | awk '{sum+=$1} END {print sum+0}')
    echo "${ver}|${date}|${total}" >> "$VERSIONS_FILE.final"
done < "$VERSIONS_FILE"

# Sort by date (most recent first)
sort -t'|' -k2 -r "$VERSIONS_FILE.final" > "$VERSIONS_FILE.sorted"
mv "$VERSIONS_FILE.sorted" "$VERSIONS_FILE.final"

# Platform calculations
MACOS_DL=$(grep -iE 'darwin|macos|apple|\.dmg' "$ASSETS_FILE" 2>/dev/null | cut -d'|' -f1 | awk '{sum+=$1} END {print sum+0}')
LINUX_DL=$(grep -iE 'linux|\.deb|\.rpm|\.appimage' "$ASSETS_FILE" 2>/dev/null | grep -iv 'darwin' | cut -d'|' -f1 | awk '{sum+=$1} END {print sum+0}')
WINDOWS_DL=$(grep -iE 'windows|\.exe|\.msi' "$ASSETS_FILE" 2>/dev/null | cut -d'|' -f1 | awk '{sum+=$1} END {print sum+0}')
OTHER_PLAT_DL=$((TOTAL_DOWNLOADS - MACOS_DL - LINUX_DL - WINDOWS_DL))
[ "$OTHER_PLAT_DL" -lt 0 ] && OTHER_PLAT_DL=0

# Architecture calculations
ARM64_DL=$(grep -iE 'arm64|aarch64' "$ASSETS_FILE" 2>/dev/null | cut -d'|' -f1 | awk '{sum+=$1} END {print sum+0}')
X86_DL=$(grep -iE 'amd64|x86_64|x64' "$ASSETS_FILE" 2>/dev/null | cut -d'|' -f1 | awk '{sum+=$1} END {print sum+0}')
OTHER_ARCH_DL=$((TOTAL_DOWNLOADS - ARM64_DL - X86_DL))
[ "$OTHER_ARCH_DL" -lt 0 ] && OTHER_ARCH_DL=0

# Print report
echo ""
echo "======================================================================"
echo "              GITHUB RELEASE DOWNLOAD METRICS REPORT"
echo "              Repository: ${REPO}"
echo "              Generated: $(date '+%Y-%m-%d %H:%M:%S')"
echo "======================================================================"
echo ""
echo "----------------------------------------------------------------------"
echo "SUMMARY"
echo "----------------------------------------------------------------------"
printf "  Total Releases:     %d\n" "$TOTAL_RELEASES"
printf "  Total Downloads:    %d\n" "$TOTAL_DOWNLOADS"

if [ "$TOTAL_RELEASES" -gt 0 ]; then
    AVG=$(awk "BEGIN {printf \"%.1f\", $TOTAL_DOWNLOADS / $TOTAL_RELEASES}")
    printf "  Avg per Release:    %s\n" "$AVG"
fi

echo ""
echo "----------------------------------------------------------------------"
echo "DOWNLOADS BY PLATFORM"
echo "----------------------------------------------------------------------"

PLAT_TOTAL=$((MACOS_DL + LINUX_DL + WINDOWS_DL + OTHER_PLAT_DL))

print_bar() {
    local name="$1"
    local count="$2"
    local total="$3"
    local width=12

    if [ "$total" -gt 0 ] && [ "$count" -gt 0 ]; then
        local pct=$((count * 100 / total))
        local bar_len=$((pct / 2))
        local bar=""
        for ((i=0; i<bar_len; i++)); do bar+="#"; done
        printf "  %-${width}s %8d (%3d%%) %s\n" "$name" "$count" "$pct" "$bar"
    fi
}

if [ "$PLAT_TOTAL" -gt 0 ]; then
    print_bar "macOS" "$MACOS_DL" "$PLAT_TOTAL"
    print_bar "Linux" "$LINUX_DL" "$PLAT_TOTAL"
    print_bar "Windows" "$WINDOWS_DL" "$PLAT_TOTAL"
    print_bar "Other" "$OTHER_PLAT_DL" "$PLAT_TOTAL"
else
    echo "  No platform data available"
fi

echo ""
echo "----------------------------------------------------------------------"
echo "DOWNLOADS BY ARCHITECTURE"
echo "----------------------------------------------------------------------"

ARCH_TOTAL=$((ARM64_DL + X86_DL + OTHER_ARCH_DL))

print_bar_wide() {
    local name="$1"
    local count="$2"
    local total="$3"

    if [ "$total" -gt 0 ] && [ "$count" -gt 0 ]; then
        local pct=$((count * 100 / total))
        local bar_len=$((pct / 2))
        local bar=""
        for ((i=0; i<bar_len; i++)); do bar+="#"; done
        printf "  %-16s %8d (%3d%%) %s\n" "$name" "$count" "$pct" "$bar"
    fi
}

if [ "$ARCH_TOTAL" -gt 0 ]; then
    print_bar_wide "ARM64" "$ARM64_DL" "$ARCH_TOTAL"
    print_bar_wide "x86_64/AMD64" "$X86_DL" "$ARCH_TOTAL"
    print_bar_wide "Other/Universal" "$OTHER_ARCH_DL" "$ARCH_TOTAL"
else
    echo "  No architecture data available"
fi

echo ""
echo "----------------------------------------------------------------------"
echo "DOWNLOADS BY VERSION (excluding checksum files)"
echo "----------------------------------------------------------------------"
printf "  %-12s %-12s %10s  %s\n" "Version" "Date" "Downloads" "Bar"
echo "  --------------------------------------------------------"

# Get max downloads for bar scaling
MAX_DL=$(cut -d'|' -f3 "$VERSIONS_FILE.final" 2>/dev/null | sort -rn | head -1)
MAX_DL=${MAX_DL:-1}
[ "$MAX_DL" -eq 0 ] && MAX_DL=1

COUNT=0
while IFS='|' read -r version date downloads; do
    [ "$COUNT" -ge 15 ] && break
    [ -z "$version" ] && continue

    date=${date:-Unknown}
    downloads=${downloads:-0}

    if [ "$MAX_DL" -gt 0 ] && [ "$downloads" -gt 0 ]; then
        bar_len=$((downloads * 20 / MAX_DL))
    else
        bar_len=0
    fi

    bar=""
    for ((i=0; i<bar_len; i++)); do bar+="#"; done

    printf "  %-12s %-12s %10d  %s\n" "$version" "$date" "$downloads" "$bar"
    ((COUNT++))
done < "$VERSIONS_FILE.final"

if [ "$TOTAL_RELEASES" -gt 15 ]; then
    echo "  ... and $((TOTAL_RELEASES - 15)) more releases"
fi

echo ""
echo "----------------------------------------------------------------------"
echo "TOP DOWNLOADED ASSETS (All Time, excluding checksums)"
echo "----------------------------------------------------------------------"
printf "  %-45s %-10s %10s\n" "Asset Name" "Version" "Downloads"
echo "  -------------------------------------------------------------------"

# Sort assets by downloads and show top 10
sort -t'|' -k1 -rn "$ASSETS_FILE" | head -10 | while IFS='|' read -r downloads name version; do
    [ -z "$name" ] && continue
    [ "$downloads" -eq 0 ] 2>/dev/null && continue

    # Truncate long names
    if [ ${#name} -gt 44 ]; then
        name="${name:0:41}..."
    fi

    printf "  %-45s %-10s %10d\n" "$name" "$version" "$downloads"
done

echo ""
echo "======================================================================"
echo "END OF REPORT"
echo "======================================================================"

# Cleanup
rm -rf "$TEMP_DIR"

#!/usr/bin/env bash
# Combine every Playwright showcase video.webm under test-results/ into a
# single mp4 capped at MAX_SECONDS (default 120s). Speed-up is computed
# so the final clip is always ~MAX_SECONDS regardless of input count.
#
# Two layouts:
#
#   - concat (default): videos play back-to-back. factor = max(1, sum / cap).
#   - grid (GRID=COLSxROWS): videos play in parallel in a fixed grid.
#     Shorter cells are tail-padded with their last frame so every cell
#     ends at the same time. factor = max(1, longest / cap).
#     If video count > COLS*ROWS, the extras are dropped (warn).
#     If video count < COLS*ROWS, empty cells are filled with black.
#
# Optional per-cell / per-segment title overlay (SHOW_TITLES=1). Title
# text is derived from the video's parent directory name (Playwright
# names it after the spec + test title).
#
# Output is H.264 mp4 (libx264, crf 18, slow preset). Re-encode is
# unavoidable because we apply a setpts filter; stream-copy is not
# compatible with filter graphs.
#
# Usage:
#   ./scripts/combine-showcase-videos.sh                                  # concat, 120s cap, no titles
#   ./scripts/combine-showcase-videos.sh out.mp4                          # custom output
#   MAX_SECONDS=60 ./scripts/combine-showcase-videos.sh
#   GRID=2x2 ./scripts/combine-showcase-videos.sh                         # 2x2 grid
#   GRID=3x3 SHOW_TITLES=1 ./scripts/combine-showcase-videos.sh           # 3x3 grid + per-cell titles
#   SHOW_TITLES=1 ./scripts/combine-showcase-videos.sh                    # concat + per-segment titles
#   CRF=23 PRESET=fast ./scripts/combine-showcase-videos.sh               # cheaper / lower-quality encode
#   TARGET_W=1920 TARGET_H=1080 ./scripts/combine-showcase-videos.sh      # output canvas size
#   FONT=/path/to/font.ttf ./scripts/combine-showcase-videos.sh           # override drawtext font
#
# Order is filesystem-sorted (Playwright's per-test dir prefix sorts
# stably enough for a deterministic playback order across runs).

set -euo pipefail

# ffmpeg opens one fd per input. Grid/concat-with-titles modes pass every
# recording as its own -i, so a ~250-spec showcase blows past macOS's
# default 256 soft fd limit ("Too many open files"). Raise to the lesser
# of 8192 and the current hard limit. macOS hard limit is typically
# 10240+ (`ulimit -Hn`).
hard="$(ulimit -Hn 2>/dev/null || echo 8192)"
if [[ "${hard}" == "unlimited" ]]; then
  ulimit -n 8192 || true
elif [[ "${hard}" -gt 8192 ]]; then
  ulimit -n 8192 || true
else
  ulimit -n "${hard}" || true
fi

MAX_SECONDS="${MAX_SECONDS:-120}"
CRF="${CRF:-14}"
PRESET="${PRESET:-slow}"
GRID="${GRID:-}"
SHOW_TITLES="${SHOW_TITLES:-0}"
# Playwright videos are captured at 1440x900 (see playwright.showcase.config.ts).
# Keep the target at native resolution: upscaling adds no detail and just
# bloats encode time.
TARGET_W="${TARGET_W:-1440}"
TARGET_H="${TARGET_H:-900}"
FONT="${FONT:-}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS="${ROOT}/test-results"
OUT="${1:-${RESULTS}/showcase-combined.mp4}"

if [[ ! -d "${RESULTS}" ]]; then
  echo "no test-results/ directory at ${RESULTS}" >&2
  exit 1
fi

for bin in ffmpeg ffprobe awk; do
  if ! command -v "${bin}" >/dev/null 2>&1; then
    echo "${bin} not found on PATH" >&2
    exit 1
  fi
done

# Default fonts per platform if drawtext is requested but FONT not set.
if [[ "${SHOW_TITLES}" == "1" && -z "${FONT}" ]]; then
  for candidate in \
    "/System/Library/Fonts/Supplemental/Arial.ttf" \
    "/System/Library/Fonts/Helvetica.ttc" \
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf" \
    "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf"; do
    if [[ -f "${candidate}" ]]; then
      FONT="${candidate}"
      break
    fi
  done
  if [[ -z "${FONT}" ]]; then
    echo "SHOW_TITLES=1 but no usable font found; set FONT=/path/to/font.ttf" >&2
    exit 1
  fi
fi

# Escape a string for drawtext: backslash, colon, percent, single quote.
escape_drawtext() {
  local s="$1"
  s="${s//\\/\\\\}"
  s="${s//:/\\:}"
  s="${s//\'/\\\\\'}"
  s="${s//%/\\%}"
  printf '%s' "${s}"
}

# Turn "test-results/tests-live-golden-path-create-view-delete.../video.webm"
# into "golden path: create view delete...". Truncated for overlay sanity.
derive_title() {
  local video="$1"
  local dir
  dir="$(basename "$(dirname "${video}")")"
  # Drop common Playwright prefix.
  dir="${dir#tests-}"
  dir="${dir#live-}"
  # Dashes -> spaces. Cap to 80 chars.
  dir="${dir//-/ }"
  if [[ "${#dir}" -gt 80 ]]; then
    dir="${dir:0:77}..."
  fi
  printf '%s' "${dir}"
}

# Collect videos + per-file metadata.
videos=()
titles=()
durations=()
while IFS= read -r -d '' video; do
  videos+=("${video}")
  titles+=("$(derive_title "${video}")")
  dur="$(ffprobe -v error -show_entries format=duration -of default=nokey=1:noprint_wrappers=1 "${video}" || echo 0)"
  durations+=("${dur}")
done < <(find "${RESULTS}" -type f -name 'video.webm' -print0 | sort -z)

count="${#videos[@]}"
if [[ "${count}" -eq 0 ]]; then
  echo "no video.webm files found under ${RESULTS}" >&2
  exit 1
fi

# Common drawtext fragment (omit text= so we can plug in per-call).
drawtext_style() {
  printf "fontfile='%s':fontcolor=white:fontsize=22:box=1:boxcolor=black@0.55:boxborderw=10:x=24:y=h-th-24" "${FONT}"
}

if [[ -n "${GRID}" ]]; then
  # ---- grid mode (streaming lanes) ------------------------------------
  # Each cell is an independent lane. Videos are distributed round-robin
  # across lanes so cell 0 plays inputs 0,N,2N,...; cell 1 plays 1,N+1,...
  # When a lane's current video ends, the next one starts immediately:
  # cells don't wait for each other. The overall clip ends when every
  # lane has exhausted its queue; shorter lanes are tail-padded with
  # their last frame so xstack can run to that final tick.
  #
  # Round-robin (vs chunked) keeps all lanes lively at once and avoids
  # the "one lane finished 30s ago" dead-air effect you'd get if cell 0
  # got the first N/cells videos.
  if [[ ! "${GRID}" =~ ^([0-9]+)x([0-9]+)$ ]]; then
    echo "GRID must be COLSxROWS (e.g. 2x2), got '${GRID}'" >&2
    exit 1
  fi
  cols="${BASH_REMATCH[1]}"
  rows="${BASH_REMATCH[2]}"
  cells=$((cols * rows))
  if (( cells < 2 )); then
    echo "GRID must total at least 2 cells; use concat mode (no GRID) for single-cell" >&2
    exit 1
  fi

  # Cell dimensions; force even numbers (libx264 requires even).
  cell_w=$(( TARGET_W / cols ))
  cell_h=$(( TARGET_H / rows ))
  (( cell_w % 2 == 1 )) && cell_w=$(( cell_w - 1 ))
  (( cell_h % 2 == 1 )) && cell_h=$(( cell_h - 1 ))

  # xstack layout: cells are uniform, compute once.
  layout=""
  for (( r=0; r<rows; r++ )); do
    for (( c=0; c<cols; c++ )); do
      xexpr=""
      yexpr=""
      for (( k=0; k<c; k++ )); do
        xexpr+="+w${k}"
      done
      for (( k=0; k<r; k++ )); do
        yexpr+="+h$(( k * cols ))"
      done
      xexpr="${xexpr#+}"
      yexpr="${yexpr#+}"
      [[ -z "${xexpr}" ]] && xexpr="0"
      [[ -z "${yexpr}" ]] && yexpr="0"
      [[ -n "${layout}" ]] && layout+="|"
      layout+="${xexpr}_${yexpr}"
    done
  done

  # Round-robin lane assignment + per-lane duration sum.
  # lane_count[k]: how many videos in lane k.
  # lane_total[k]: sum of their durations.
  lane_indices_lookup=""  # space-separated "lane_k:idx_in_videos" pairs, ordered.
  lane_count=()
  lane_total=()
  for (( k=0; k<cells; k++ )); do
    lane_count[k]=0
    lane_total[k]=0
  done
  for (( i=0; i<count; i++ )); do
    k=$(( i % cells ))
    lane_count[k]=$(( lane_count[k] + 1 ))
    lane_total[k]="$(awk -v a="${lane_total[k]}" -v b="${durations[$i]}" 'BEGIN { printf "%.6f", a + b }')"
  done

  # Lane max = max over lane totals. xstack needs every lane to reach
  # that timestamp, so shorter lanes tail-pad to lane_max.
  lane_max=0
  for (( k=0; k<cells; k++ )); do
    lt="${lane_total[k]}"
    lane_max="$(awk -v a="${lane_max}" -v b="${lt}" 'BEGIN { print (b > a) ? b : a }')"
  done

  factor="$(awk -v t="${lane_max}" -v m="${MAX_SECONDS}" 'BEGIN { f = t / m; if (f < 1) f = 1; printf "%.6f", f }')"

  echo "grid ${cols}x${rows} streaming lanes, ${count} video(s) across ${cells} lane(s), longest lane=${lane_max}s, cap=${MAX_SECONDS}s, speed=${factor}x -> ${OUT}"

  # Build inputs list (ffmpeg input order = videos[] order; lane access
  # uses [${i}:v] for the i-th video).
  inputs=()
  for i in "${!videos[@]}"; do
    inputs+=("-i" "${videos[$i]}")
  done

  # Build filter graph.
  filter=""
  stack_inputs=""
  for (( k=0; k<cells; k++ )); do
    lc="${lane_count[k]}"
    lt="${lane_total[k]}"

    # Per-video chain: scale+pad to cell dims, optional title overlay.
    # Emits labels [v_k_0], [v_k_1], ... that the concat filter ties together.
    member_inputs=""
    member_idx=0
    for (( i=k; i<count; i+=cells )); do
      chain="[${i}:v]scale=${cell_w}:${cell_h}:force_original_aspect_ratio=decrease,pad=${cell_w}:${cell_h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1"
      if [[ "${SHOW_TITLES}" == "1" ]]; then
        title_escaped="$(escape_drawtext "${titles[$i]}")"
        chain+=",drawtext=$(drawtext_style):text='${title_escaped}'"
      fi
      chain+="[v_${k}_${member_idx}];"
      filter+="${chain}"
      member_inputs+="[v_${k}_${member_idx}]"
      member_idx=$(( member_idx + 1 ))
    done

    if (( lc == 0 )); then
      # Empty lane (only when cells > count). Pure black for lane_max.
      filter+="color=c=black:s=${cell_w}x${cell_h}:d=${lane_max}[lane${k}];"
    else
      # Concat the lane's members, then tail-pad to lane_max.
      pad_delta="$(awk -v m="${lane_max}" -v t="${lt}" 'BEGIN { x = m - t; if (x < 0) x = 0; printf "%.6f", x }')"
      if (( lc == 1 )); then
        # concat=n=1 is illegal; just relabel.
        filter+="${member_inputs}null,tpad=stop_mode=clone:stop_duration=${pad_delta}[lane${k}];"
      else
        filter+="${member_inputs}concat=n=${lc}:v=1:a=0,tpad=stop_mode=clone:stop_duration=${pad_delta}[lane${k}];"
      fi
    fi

    stack_inputs+="[lane${k}]"
  done

  filter+="${stack_inputs}xstack=inputs=${cells}:layout=${layout}[stacked];"
  filter+="[stacked]setpts=PTS/${factor},format=yuv420p[out]"

  ffmpeg -y "${inputs[@]}" \
    -filter_complex "${filter}" \
    -map "[out]" \
    -an \
    -c:v libx264 -preset "${PRESET}" -crf "${CRF}" \
    -movflags +faststart \
    "${OUT}"

else
  # ---- concat mode ------------------------------------------------------
  total=0
  for d in "${durations[@]}"; do
    total="$(awk -v a="${total}" -v b="${d}" 'BEGIN { printf "%.6f", a + b }')"
  done
  factor="$(awk -v t="${total}" -v m="${MAX_SECONDS}" 'BEGIN { f = t / m; if (f < 1) f = 1; printf "%.6f", f }')"

  echo "concat ${count} video(s), total=${total}s, cap=${MAX_SECONDS}s, speed=${factor}x -> ${OUT}"

  if [[ "${SHOW_TITLES}" == "1" ]]; then
    # Use filter_complex concat=, drawtext per input before concat so
    # each segment carries its own title for its own duration.
    inputs=()
    filter=""
    for i in "${!videos[@]}"; do
      inputs+=("-i" "${videos[$i]}")
      title_escaped="$(escape_drawtext "${titles[$i]}")"
      filter+="[${i}:v]scale=${TARGET_W}:${TARGET_H}:force_original_aspect_ratio=decrease,pad=${TARGET_W}:${TARGET_H}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,drawtext=$(drawtext_style):text='${title_escaped}'[s${i}];"
    done
    concat_inputs=""
    for (( i=0; i<count; i++ )); do
      concat_inputs+="[s${i}]"
    done
    filter+="${concat_inputs}concat=n=${count}:v=1:a=0[joined];"
    filter+="[joined]setpts=PTS/${factor},format=yuv420p[out]"

    ffmpeg -y "${inputs[@]}" \
      -filter_complex "${filter}" \
      -map "[out]" \
      -an \
      -c:v libx264 -preset "${PRESET}" -crf "${CRF}" \
      -movflags +faststart \
      "${OUT}"
  else
    # Cheap path: concat demuxer + setpts filter. One re-encode, no
    # per-input scaling so input resolution must match across files
    # (the showcase config pins it).
    LIST="$(mktemp -t showcase-concat.XXXXXX)"
    trap 'rm -f "${LIST}"' EXIT
    for video in "${videos[@]}"; do
      escaped="${video//\'/\'\\\'\'}"
      printf "file '%s'\n" "${escaped}" >>"${LIST}"
    done

    ffmpeg -y -f concat -safe 0 -i "${LIST}" \
      -filter:v "setpts=PTS/${factor},format=yuv420p" \
      -an \
      -c:v libx264 -preset "${PRESET}" -crf "${CRF}" \
      -movflags +faststart \
      "${OUT}"
  fi
fi

echo "done -> ${OUT}"

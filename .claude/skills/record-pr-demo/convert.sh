#!/usr/bin/env bash
# Usage: convert.sh <clip.webm> <out-dir>
# Writes <out-dir>/<clip>.mp4 (H.264, faststart) plus contact sheets
# (one frame every 2 s, 3x3 per sheet) under <out-dir>/frames/ for checking the clip.
set -euo pipefail
src=$1 out=$2
name=$(basename "$src" .webm)
mkdir -p "$out/frames"
ffmpeg -loglevel error -y -i "$src" -c:v libx264 -pix_fmt yuv420p -crf 22 -preset slow \
  -vf "scale=trunc(iw/2)*2:trunc(ih/2)*2" -movflags +faststart "$out/$name.mp4"
rm -f "$out/frames/$name"-[0-9][0-9].png
ffmpeg -loglevel error -y -i "$out/$name.mp4" -vf "fps=1/2,scale=640:-1,tile=3x3" "$out/frames/$name-%02d.png"
secs=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$out/$name.mp4")
echo "$name: ${secs%.*}s, mp4 $(du -h "$out/$name.mp4" | cut -f1)"
ls "$out/frames/$name"-[0-9][0-9].png

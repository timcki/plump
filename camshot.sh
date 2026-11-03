#!/usr/bin/env bash
# camshot.sh: grab one still from a macOS camera (built-in or iPhone
# Continuity Camera) and print the saved path, for photographing the panel
# while debugging rendering.
#
# needs camera permission for the process tree it runs under: works from a
# terminal app (or a claude CLI inside one), hangs inside AVFoundation from
# the Claude desktop app, whose embedded bundle can't request camera access.
set -euo pipefail

usage() {
    cat <<'EOF'
usage: ./camshot.sh [-d NAME] [-o FILE] [-c WxH+X+Y] [-w FRAMES] [-t SECS] [-l]

  -d NAME    camera name substring, case-insensitive (default: the first
             Continuity Camera, else the first built-in camera)
  -o FILE    output .jpg/.png (default: target/camshot/<timestamp>.jpg)
  -c GEOM    crop to a W x H box whose top-left corner is X,Y, in the
             camera's own pixels (take one uncropped shot to find them)
  -w FRAMES  frames to drop first so exposure and focus settle (default 45)
  -t SECS    give up after this long (default 20)
  -l         list video devices and exit

prints the saved file path on stdout; diagnostics go to stderr.
EOF
}

die() { echo "camshot: $*" >&2; exit 1; }

device_name=""
output=""
crop=""
warmup=45
time_limit=20
list_only=false

while getopts ":d:o:c:w:t:lh" opt; do
    case "$opt" in
        d) device_name=$OPTARG ;;
        o) output=$OPTARG ;;
        c) crop=$OPTARG ;;
        w) warmup=$OPTARG ;;
        t) time_limit=$OPTARG ;;
        l) list_only=true ;;
        h) usage; exit 0 ;;
        :) die "-$OPTARG needs a value" ;;
        *) usage >&2; exit 2 ;;
    esac
done

command -v ffmpeg >/dev/null || die "ffmpeg not found"
[[ $warmup =~ ^[0-9]+$ ]] || die "-w must be a frame count"
[[ $time_limit =~ ^[0-9]+$ ]] || die "-t must be whole seconds"

if [[ -n $crop ]]; then
    [[ $crop =~ ^([0-9]+)x([0-9]+)\+([0-9]+)\+([0-9]+)$ ]] ||
        die "-c must look like WxH+X+Y, e.g. 600x900+560+700"
    crop_w=$((10#${BASH_REMATCH[1]})) crop_h=$((10#${BASH_REMATCH[2]}))
    crop_x=$((10#${BASH_REMATCH[3]})) crop_y=$((10#${BASH_REMATCH[4]}))
    (( crop_w > 0 && crop_h > 0 )) || die "-c width and height must be non-zero"
fi

# "index<TAB>name" per video device; screen-capture inputs are dropped
list_video_devices() {
    # -list_devices always exits non-zero ("Error opening input")
    { ffmpeg -hide_banner -f avfoundation -list_devices true -i "" 2>&1 || true; } |
        awk '/video devices:/ { video = 1; next }
             /audio devices:/ { video = 0 }
             video && match($0, /\] \[[0-9]+\] /) {
                 entry = substr($0, RSTART + 3)
                 index_end = index(entry, "]")
                 name = substr(entry, index_end + 2)
                 if (name !~ /^Capture screen/)
                     printf "%s\t%s\n", substr(entry, 1, index_end - 1), name
             }'
}

devices=$(list_video_devices)
[[ -n $devices ]] || die "no cameras found (is camera permission granted?)"

if $list_only; then
    printf '%s\n' "$devices"
    exit 0
fi

# continuity cameras show up as "<phone name> Camera"; desk view is a
# separate, cropped device that should only be picked by name
pick_device() {
    local needle
    if [[ -n $device_name ]]; then
        needle=$(printf '%s' "$device_name" | tr '[:upper:]' '[:lower:]')
        printf '%s\n' "$devices" | awk -F'\t' -v n="$needle" 'index(tolower($2), n) { print; exit }'
        return
    fi
    printf '%s\n' "$devices" | awk -F'\t' '
        $2 !~ /FaceTime|Desk View/ && !picked { print; picked = 1 }
        $2 ~ /FaceTime/ && fallback == "" { fallback = $0 }
        END { if (!picked && fallback != "") print fallback }'
}

choice=$(pick_device)
[[ -n $choice ]] || die "no camera matches '${device_name}'; available:"$'\n'"$devices"
device_index=${choice%%$'\t'*}
device_label=${choice#*$'\t'}

if [[ -z $output ]]; then
    repo_root=$(cd "$(dirname "$0")" && pwd)
    output="$repo_root/target/camshot/$(date +%Y%m%d-%H%M%S).jpg"
fi
mkdir -p "$(dirname "$output")"

echo "camshot: capturing from [$device_index] $device_label" >&2

# a crop is cut from a lossless full frame in a second pass, once the
# camera's real size is known to check the box against
capture=$output
if [[ -n $crop ]]; then
    capture=$(mktemp "${TMPDIR:-/tmp}/camshot.XXXXXX").png
    trap 'rm -f "$capture" "${capture%.png}"' EXIT
fi

# nv12: avfoundation rejects ffmpeg's default yuv420p and then never
# delivers a frame. -k: a capture blocked on a permission check can ignore
# SIGTERM.
if ! timeout -k 2 "$time_limit" ffmpeg -hide_banner -loglevel error -nostdin -y \
        -f avfoundation -pixel_format nv12 -framerate 30 -i "${device_index}:none" \
        -vf "select=gte(n\,${warmup})" -frames:v 1 -update 1 -q:v 2 "$capture" >&2; then
    die "capture failed or timed out after ${time_limit}s (camera permission for this terminal? phone nearby and locked?)"
fi
[[ -s $capture ]] || die "ffmpeg wrote no image"

if [[ -n $crop ]]; then
    command -v ffprobe >/dev/null || die "ffprobe not found (needed for -c)"
    frame_size=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height \
        -of csv=p=0:s=x "$capture")
    frame_w=${frame_size%x*} frame_h=${frame_size#*x}
    (( crop_x + crop_w <= frame_w && crop_y + crop_h <= frame_h )) ||
        die "crop ${crop} falls outside the ${frame_size} frame"
    ffmpeg -hide_banner -loglevel error -nostdin -y -i "$capture" \
        -vf "crop=${crop_w}:${crop_h}:${crop_x}:${crop_y}" -update 1 -q:v 2 "$output" >&2 ||
        die "crop failed"
fi

printf '%s\n' "$output"

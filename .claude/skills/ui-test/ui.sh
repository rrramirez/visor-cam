#!/usr/bin/env bash
# Drives a Visor test instance for the ui-test skill (macOS). Run from the
# repo root:  .claude/skills/ui-test/ui.sh <command> [args]
#
#   check                      can this session send input? (prints true/false)
#   prompt                     ask macOS to show the permission request
#   launch [--dmg] [ARGS...]   start Visor with the test home (debug build, or
#                              dist/Visor.app with --dmg); ARGS go to visor,
#                              e.g. --edit "$(ui.sh home)/Movies/Visor/Demo"
#   sample NAME SECONDS [WxH]  make a test recording (screen + mic) in the test home
#   video PATH SECONDS [WxH] [--rotate DEG] [--silent]
#                              make a video file to add, as another app would
#   pid                        the test instance's pid
#   windows                    its windows: id x y w h layer title
#   front                      bring it to the front (do this before each input)
#   shot WINDOW OUT.png [W]    screenshot one window (resized to W px wide, default 1000)
#   click|drag|scroll|key|type ...   input, see tools/input.swift
#   dragsrc FILE...            a small window to drag those files from (prints
#                              its id x y w h, as `windows` does)
#   cli ARGS...                run the visor command line against the test instance
#   home                       print the test home folder
#   stop                       quit the test instance and drag sources
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
dir="$root/target/ui-test"
home="$dir/home"
tools="$(cd "$(dirname "$0")/tools" && pwd)"
debug_bin="$root/target/debug/visor"
dmg_bin="$root/dist/Visor.app/Contents/MacOS/visor"

build_tool() {
    local name=$1
    mkdir -p "$dir"
    if [ ! -x "$dir/$name" ] || [ "$tools/$name.swift" -nt "$dir/$name" ]; then
        swiftc -O "$tools/$name.swift" -o "$dir/$name" >/dev/null
    fi
}

input() { build_tool input; "$dir/input" "$@"; }

ensure_home() {
    local settings="$home/Library/Application Support/Visor/settings.json"
    mkdir -p "$home/Movies/Visor" "$(dirname "$settings")"
    if [ ! -f "$settings" ]; then
        cat >"$settings" <<EOF
{
  "save_dir": "$home/Movies/Visor",
  "fps": 30,
  "countdown_secs": 3,
  "show_cursor": false,
  "shortcuts": false,
  "agent_control": true,
  "folded_sections": []
}
EOF
    fi
}

# Only processes started with the test home: never the user's own Visor.
test_pids() {
    ps eww -ax -o pid=,command= | grep -F "HOME=$home " | grep -E "visor( |$)" | grep -v grep |
        awk '{print $1}' || true
}

case "${1:-}" in
check) input trusted ;;
prompt) input prompt ;;
home) ensure_home; echo "$home" ;;
launch)
    shift
    ensure_home
    bin=$debug_bin
    if [ "${1:-}" = "--dmg" ]; then bin=$dmg_bin; shift; else cargo build -q -p visor; fi
    [ -x "$bin" ] || { echo "no $bin (run scripts/package-macos.sh for --dmg)" >&2; exit 1; }
    [ -z "$(test_pids)" ] || { echo "a test instance is running; ui.sh stop first" >&2; exit 1; }
    # VISOR_DEBUG_UNPROTECTED lets screenshots see Visor's overlays.
    (HOME="$home" VISOR_DEBUG_UNPROTECTED=1 nohup "$bin" "$@" >"$dir/visor.log" 2>&1 &)
    for _ in $(seq 1 40); do
        pid=$(test_pids | head -1)
        [ -n "$pid" ] && input windows "$pid" | grep -q . && { echo "$pid"; exit 0; }
        sleep 0.25
    done
    echo "Visor didn't open a window; see $dir/visor.log" >&2
    exit 1
    ;;
sample)
    ensure_home
    name=$2 secs=$3 size=${4:-1280x720}
    out="$home/Movies/Visor/$name"
    rm -rf "$out" && mkdir -p "$out"
    ffmpeg -loglevel error -y -f lavfi -i "testsrc=size=$size:rate=30:duration=$secs" \
        -c:v libx264 -pix_fmt yuv420p "$out/screen.mp4"
    ffmpeg -loglevel error -y -f lavfi -i "sine=f=440:duration=$secs" -c:a aac "$out/mic.m4a"
    python3 - "$out" "$name" "$secs" "$size" <<'EOF'
import json, sys
out, name, secs, size = sys.argv[1], sys.argv[2], float(sys.argv[3]), sys.argv[4]
w, h = (int(v) for v in size.split("x"))
d = int(secs * 1_000_000)
json.dump({
    "version": 1, "title": name, "created_at": "2026-01-01T00:00:00Z", "duration_us": d,
    "tracks": {"screen": {"file": "screen.mp4", "width": w, "height": h, "fps": 30},
               "camera": None, "mic": {"file": "mic.m4a"}, "system": None},
    "edit": {"keep": [[0, d]]},
}, open(f"{out}/project.json", "w"), indent=1)
EOF
    echo "$out"
    ;;
video)
    path=$2 secs=$3 size=1280x720 rotate="" silent=""
    shift 3
    while [ $# -gt 0 ]; do
        case $1 in
        --rotate) rotate=$2; shift ;;
        --silent) silent=1 ;;
        *) size=$1 ;;
        esac
        shift
    done
    tmp="$dir/video-tmp.${path##*.}"
    mkdir -p "$dir"
    if [ -n "$silent" ]; then
        ffmpeg -loglevel error -y -f lavfi -i "testsrc2=size=$size:rate=30:duration=$secs" \
            -c:v libx264 -pix_fmt yuv420p -movflags +faststart "$tmp"
    else
        ffmpeg -loglevel error -y -f lavfi -i "testsrc=size=$size:rate=30:duration=$secs" \
            -f lavfi -i "sine=f=440:duration=$secs" -c:v libx264 -pix_fmt yuv420p -c:a aac -movflags +faststart "$tmp"
    fi
    if [ -n "$rotate" ]; then
        # FFmpeg's option turns counterclockwise; a phone held upright is -90.
        ffmpeg -loglevel error -y -display_rotation "$rotate" -i "$tmp" -c copy "$path"
        rm -f "$tmp"
    else
        mv "$tmp" "$path"
    fi
    echo "$path"
    ;;
pid) test_pids | head -1 ;;
windows) input windows "$(test_pids | head -1)" ;;
front) input front "$(test_pids | head -1)" ;;
shot)
    screencapture -x -o -l "$2" "$3"
    sips -Z "${4:-1000}" "$3" >/dev/null
    echo "$3"
    ;;
click | drag | scroll | key | type) input "$@" ;;
dragsrc)
    shift
    build_tool dragsrc
    ("$dir/dragsrc" "$@" >/dev/null 2>&1 &)
    sleep 1.5
    input windows "$(pgrep -f "$dir/dragsrc" | tail -1)" | grep "drag source"
    ;;
cli) shift; ensure_home; HOME="$home" "$debug_bin" "$@" ;;
stop)
    for p in $(test_pids); do kill "$p"; done
    pkill -f "$dir/dragsrc" || true
    ;;
*) sed -n '2,30p' "$0"; exit 2 ;;
esac

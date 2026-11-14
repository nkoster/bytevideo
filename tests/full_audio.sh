#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
printf 'test' > "$work/input.bin"
./target/release/bytevideo encode "$work/input.bin" "$work/full.mp4" --full-audio
python3 - "$work/full.mp4" <<'PY'
import json, subprocess, sys
s = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_streams', '-of', 'json', sys.argv[1]]))['streams']
v, a = (next(x for x in s if x['codec_type'] == k) for k in ['video', 'audio'])
assert abs(float(v['duration']) - 173.6) < 1/48000
assert abs(float(v['duration']) - float(a['duration'])) < 1/48000
PY
ffmpeg -v error -i "$work/full.mp4" -vn -c:a aac -b:a 128k "$work/audio.m4a"
./target/release/bytevideo decode "$work/audio.m4a" "$work/recovered.bin"
cmp "$work/input.bin" "$work/recovered.bin"
echo 'Full audio backup check passed'

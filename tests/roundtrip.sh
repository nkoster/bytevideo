#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
bin=./target/release/bytevideo
python3 - "$work/input.bin" <<'PY'
import random, sys
open(sys.argv[1], 'wb').write(random.Random(42).randbytes(200000))
PY
"$bin" encode "$work/input.bin" "$work/original.mp4"
python3 - "$work/original.mp4" <<'PY'
import json, subprocess, sys
s = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_streams', '-of', 'json', sys.argv[1]]))['streams']
v, a = (next(x for x in s if x['codec_type'] == k) for k in ['video', 'audio'])
assert v['avg_frame_rate'] == '30/1'
assert int(v['nb_frames']) == 288
assert abs(float(v['duration']) - 9.6) < 1/48000
assert abs(float(v['duration']) - float(a['duration'])) < 1/48000
assert float(v['start_time']) == float(a['start_time']) == 0
print('Matched A/V timeline:', v['duration'], a['duration'])
PY
"$bin" decode "$work/original.mp4" "$work/plain.bin" | tee "$work/decode.log"
cmp "$work/input.bin" "$work/plain.bin"
# There are eight groups, packed two packets per frame. After 64 frames, SHA-256 is
# verified: the decoder must stop without reading either audio or later copies.
rg -q '64 video frames read; audio skipped' "$work/decode.log"
# Remove all copies of eight shard positions in all eight groups; video alone suffices.
ffmpeg -v error -i "$work/original.mp4" -vf "select='gte(mod(n,96),32)',setpts=N/(30*TB)" \
  -fps_mode vfr -an -c:v libx264 -crf 28 "$work/dropped.mp4"
"$bin" decode "$work/dropped.mp4" "$work/dropped.bin"
cmp "$work/input.bin" "$work/dropped.bin"
# Nine missing shards in group zero, eight elsewhere: video alone fails.
# Compact audio supplies parity shard 16 of group zero and makes recovery possible.
ffmpeg -v error -i "$work/original.mp4" \
  -vf "drawbox=x=0:y=0:w=iw:h=ih/2:color=black:t=fill:enable='eq(mod(n,96),64)',select='gte(mod(n,96),32)',setpts=N/(30*TB)" \
  -fps_mode vfr -an -c:v libx264 -crf 28 "$work/insufficient.mp4"
if "$bin" decode "$work/insufficient.mp4" "$work/must-not-exist.bin"; then
  echo 'Expected recovery to fail' >&2; exit 1
fi
test ! -e "$work/must-not-exist.bin"
ffmpeg -v error -i "$work/insufficient.mp4" -i "$work/original.mp4" \
  -map 0:v -map 1:a -c:v copy -c:a aac -b:a 128k "$work/combined.mp4"
"$bin" decode "$work/combined.mp4" "$work/combined.bin" | tee "$work/combined.log"
rg -q 'audio used' "$work/combined.log"
cmp "$work/input.bin" "$work/combined.bin"
# A burst of damage plus H.264 recompression must still recover via later copies.
ffmpeg -v error -i "$work/original.mp4" \
  -vf "drawbox=x=0:y=0:w=iw:h=ih:color=black:t=fill:enable='between(n,12,35)'" \
  -an -c:v libx264 -crf 28 "$work/damaged.mp4"
"$bin" decode "$work/damaged.mp4" "$work/damaged.bin"
cmp "$work/input.bin" "$work/damaged.bin"
# Tiny input: trimmed zero padding lets useful repair packets fit in 1.2 seconds.
printf 'test' > "$work/tiny.bin"
"$bin" encode "$work/tiny.bin" "$work/tiny.mp4"
ffmpeg -v error -i "$work/tiny.mp4" \
  -vf "drawbox=x=0:y=0:w=iw:h=ih/2:color=black:t=fill:enable='eq(mod(n,12),8)',select='gte(mod(n,12),4)',setpts=N/(30*TB)" \
  -fps_mode vfr -c:v libx264 -crf 28 -c:a aac -b:a 128k "$work/tiny-damaged.mp4"
"$bin" decode "$work/tiny-damaged.mp4" "$work/tiny-recovered.bin"
cmp "$work/tiny.bin" "$work/tiny-recovered.bin"
# Both operations preserve an existing destination if recovery/FFmpeg fails.
printf 'keep existing output' > "$work/expected-existing.bin"
cp "$work/expected-existing.bin" "$work/existing.bin"
if "$bin" decode "$work/insufficient.mp4" "$work/existing.bin"; then
  echo 'Expected recovery to fail without replacing existing output' >&2; exit 1
fi
cmp "$work/expected-existing.bin" "$work/existing.bin"
mkdir "$work/fakebin" "$work/tmp"
cat > "$work/fakebin/ffmpeg" <<'SH'
#!/usr/bin/env bash
# Simulate FFmpeg failing after creating an incomplete output.
for output do :; done
printf 'partial MP4' > "$output"
exit 1
SH
chmod +x "$work/fakebin/ffmpeg"
if PATH="$work/fakebin:$PATH" TMPDIR="$work/tmp" "$bin" encode "$work/tiny.bin" "$work/existing.bin"; then
  echo 'Expected FFmpeg failure' >&2; exit 1
fi
cmp "$work/expected-existing.bin" "$work/existing.bin"
python3 - "$work" <<'PY_CHECK'
from pathlib import Path
import sys
assert not list(Path(sys.argv[1]).rglob('.bytevideo-*')), 'Temporary workspace leaked'
PY_CHECK
echo 'All fast MP4 roundtrip checks passed'

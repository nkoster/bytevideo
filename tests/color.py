#!/usr/bin/env python3
"""Validate displayed tile colors, unchanged timing, and video-only recovery."""
import json
from pathlib import Path
import random
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
subprocess.run(["cargo", "build", "--release"], cwd=ROOT, check=True)
BIN = ROOT / "target/release/bytevideo"


def streams(path):
    return json.loads(subprocess.check_output([
        "ffprobe", "-v", "error", "-show_streams", "-of", "json", str(path)
    ]))["streams"]


with tempfile.TemporaryDirectory(prefix="bytevideo-color-") as directory:
    work = Path(directory)
    source = work / "input.bin"
    source.write_bytes(random.Random(19).randbytes(60000))  # Three groups.
    gray, color = work / "gray.mp4", work / "color.mp4"
    for path, flags in [(gray, []), (color, ["--color"])]:
        subprocess.run([str(BIN), "encode", str(source), str(path), "--gray-levels", "4", *flags], check=True)
    for kind in ["video", "audio"]:
        a, b = [next(s for s in streams(path) if s["codec_type"] == kind) for path in [gray, color]]
        assert a["duration"] == b["duration"]
        if kind == "video":
            assert a["nb_frames"] == b["nb_frames"] == "108"
            assert a["pix_fmt"] == b["pix_fmt"] == "yuv420p"
    rgb = subprocess.check_output([
        "ffmpeg", "-nostdin", "-v", "error", "-i", str(color), "-frames:v", "1",
        "-f", "rawvideo", "-pix_fmt", "rgb24", "-"
    ])
    matches, sampled = 0, 0
    for y in range(90):
        for x in range(160):
            offset = ((y * 8 + 4) * 1280 + x * 8 + 4) * 3
            pixel = rgb[offset:offset + 3]
            # Midtones show the tint most clearly; black/white are less saturated.
            if 50 < sum(pixel) / 3 < 205:
                sampled += 1
                red, green, blue = pixel
                hue = (x + y) % 3
                if hue == 0:
                    matches += red > max(green, blue)
                elif hue == 1:
                    matches += min(red, green) > blue and abs(red - green) < min(red, green) - blue
                else:
                    matches += green > max(red, blue)
    assert sampled > 1000
    assert matches / sampled > 0.95, (matches, sampled)
    print(f"Expected Rasta tint in {matches}/{sampled} sampled midtone tiles")
    # Audio removed: success must come from the existing luma-only video decoder.
    for crf in [23, 28]:
        recompressed = work / f"crf{crf}.mp4"
        subprocess.run([
            "ffmpeg", "-nostdin", "-v", "error", "-i", str(color), "-an",
            "-c:v", "libx264", "-crf", str(crf), str(recompressed)
        ], check=True)
        restored = work / f"crf{crf}.bin"
        subprocess.run([str(BIN), "decode", str(recompressed), str(restored)], check=True)
        assert restored.read_bytes() == source.read_bytes()
    # Unknown/duplicate options and encode-only flags on decode are rejected.
    for flags in [["--color", "--color"], ["--colour"]]:
        output = work / "invalid.mp4"
        result = subprocess.run([str(BIN), "encode", str(source), str(output), *flags], capture_output=True)
        assert result.returncode != 0 and not output.exists()
    result = subprocess.run([str(BIN), "decode", str(color), str(work / "invalid.bin"), "--color"], capture_output=True)
    assert result.returncode != 0

print("Color appearance, timing, CLI validation and CRF 23/28 recovery passed")

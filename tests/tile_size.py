#!/usr/bin/env python3
"""Check every tile size through H.264, without audio recovery."""
import argparse
import json
from pathlib import Path
import random
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
subprocess.run(["cargo", "build", "--release"], cwd=ROOT, check=True)
BIN = ROOT / "target/release/bytevideo"
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--levels", type=int, nargs="+", choices=range(2, 11), default=range(2, 11))
options = parser.parse_args()


def run(*args):
    return subprocess.check_output([str(arg) for arg in args], text=True)


with tempfile.TemporaryDirectory(prefix="bytevideo-tiles-") as directory:
    work = Path(directory)
    source = work / "input.bin"
    source.write_bytes(random.Random(42).randbytes(60000))  # Three groups.
    cases = [(size, levels) for levels in options.levels for size in range(2, 11)]
    for size, levels in cases:
        for color in [False, True]:
            original = work / f"tiles{size}-levels{levels}-{color}.mp4"
            flags = ["--color"] if color else []
            run(BIN, "encode", source, original, "--tile-size", size, "--gray-levels", levels, *flags)
            streams = json.loads(run(
                "ffprobe", "-v", "error", "-show_streams", "-of", "json", original
            ))["streams"]
            video, audio = [next(s for s in streams if s["codec_type"] == kind)
                            for kind in ["video", "audio"]]
            if levels in [2, 4]:
                digits = 8 if levels == 2 else 4
                capacity = (1280 // size // digits) * (720 // size)
                fragmented = capacity < 1800
                units = capacity // (256 if fragmented else 1800)
                total = 3 * 24 * 3 * (8 if fragmented else 1)
                frames = (total + units - 1) // units
            else:
                # Select the best bounded radix block, then account for packet
                # fragments filling the remainder of each frame.
                blocks = []
                for count in range(1, 8):
                    digits = 1
                    while levels ** digits < 256 ** count:
                        digits += 1
                    blocks.append((digits / count, count, digits))
                _, count, digits = min(blocks)
                capacity = ((1280 // size) * (720 // size) // digits) * count
                frames, used = 0, 0
                for _ in range(3 * 24 * 3):
                    if capacity - used < 256:
                        frames += 1
                        used = 0
                    lengths = [1800] if capacity - used >= 1800 else [256] * 8
                    for length in lengths:
                        if used + length > capacity:
                            frames += 1
                            used = 0
                        used += length
                    if used == capacity:
                        frames += 1
                        used = 0
                frames += bool(used)
            assert int(video["nb_frames"]) == frames
            assert video["avg_frame_rate"] == "30/1"
            assert abs(float(video["duration"]) - frames / 30) < 1 / 48000
            assert abs(float(video["duration"]) - float(audio["duration"])) < 1 / 48000
            silent = work / "video-only.mp4"
            run("ffmpeg", "-nostdin", "-v", "error", "-y", "-i", original,
                "-an", "-c:v", "copy", silent)
            restored = work / "restored.bin"
            message = run(BIN, "decode", silent, restored)
            assert "audio skipped" in message, message
            assert restored.read_bytes() == source.read_bytes()
            print(f"Tile size {size}, levels {levels}, color {color}: {frames} frames, video-only roundtrip OK", flush=True)

    # A one-group recording ends in a partially filled frame at these sizes.
    for data in [b"", b"test"]:
        source.write_bytes(data)
        for size in [2, 3, 5]:
            original = work / "tiny.mp4"
            run(BIN, "encode", source, original, f"--tile-size={size}")
            restored = work / "tiny.bin"
            assert "audio skipped" in run(BIN, "decode", original, restored)
            assert restored.read_bytes() == data

print("All tile sizes, gray levels, A/V timing, padding and video-only roundtrips passed")

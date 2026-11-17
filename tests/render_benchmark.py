#!/usr/bin/env python3
"""Compare original tile rendering with precomputed patterns and reused buffers."""
import argparse
import json
import os
from pathlib import Path
import random
import re
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--frames", type=int, default=5000)
parser.add_argument("--runs", type=int, default=3)
parser.add_argument("--baseline-bin", type=Path, help="optional pre-optimization release binary for end-to-end comparison")
parser.add_argument("--input-mib", type=int, default=2)
args = parser.parse_args()
if min(args.frames, args.runs, args.input_mib) <= 0:
    parser.error("frames, runs and input-mib must be positive")
root = Path(__file__).resolve().parent.parent
baseline = args.baseline_bin.resolve() if args.baseline_bin else None
os.chdir(root)
artifacts = subprocess.check_output([
    "cargo", "test", "--release", "--no-run", "--message-format=json"
], text=True)
executables = [event["executable"] for line in artifacts.splitlines()
               if (event := json.loads(line)).get("reason") == "compiler-artifact"
               and event.get("executable") and event.get("profile", {}).get("test")]
if len(executables) != 1:
    raise SystemExit("Expected one test executable")
for color in [False, True]:
    times = {"reference": [], "reuse": []}
    for run in range(args.runs):
        modes = ["reference", "reuse"] if run % 2 == 0 else ["reuse", "reference"]
        for mode in modes:
            result = subprocess.check_output([
                executables[0], "--ignored", "--exact", "tests::render_benchmark", "--nocapture"
            ], text=True, env={**os.environ, "RENDER_MODE": mode,
                              "RENDER_COLOR": str(int(color)), "RENDER_FRAMES": str(args.frames)})
            times[mode].append(float(re.search(r"seconds=([0-9.]+)", result)[1]))
    old, new = [statistics.median(times[key]) for key in ["reference", "reuse"]]
    print(f"Render color={color}: {args.frames} frames; median {old:.4f}s -> {new:.4f}s "
          f"({old / new:.2f}x, {(1 - new / old) * 100:.1f}% less time)", flush=True)

if baseline:
    subprocess.run(["cargo", "build", "--release"], check=True)
    optimized = root / "target/release/bytevideo"
    with tempfile.TemporaryDirectory(prefix="bytevideo-render-benchmark-") as directory:
        work = Path(directory)
        source = work / "input.bin"
        generator = random.Random(37)
        with source.open("wb") as target:
            for _ in range(args.input_mib * 16):
                target.write(generator.randbytes(65536))
        for color in [False, True]:
            times = {"reference": [], "reuse": []}
            for run in range(args.runs):
                order = [("reference", baseline), ("reuse", optimized)]
                if run % 2:
                    order.reverse()
                for name, binary in order:
                    output = work / f"{name}.mp4"
                    started = time.perf_counter()
                    subprocess.run([str(binary), "encode", str(source), str(output),
                                    *(["--color"] if color else [])], stdout=subprocess.DEVNULL, check=True)
                    times[name].append(time.perf_counter() - started)
                    # Validate each produced recording, outside the timed encode.
                    restored = work / "restored.bin"
                    subprocess.run([str(optimized), "decode", str(output), str(restored)],
                                   stdout=subprocess.DEVNULL, check=True)
                    assert restored.read_bytes() == source.read_bytes()
            old, new = [statistics.median(times[key]) for key in ["reference", "reuse"]]
            print(f"Encode color={color}: {args.input_mib} MiB; median {old:.4f}s -> {new:.4f}s "
                  f"({(1 - new / old) * 100:.1f}% less time); "
                  f"runs={times}", flush=True)

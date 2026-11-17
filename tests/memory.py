#!/usr/bin/env python3
"""Measure core streaming RSS in fresh processes, without FFmpeg."""
import argparse
import json
import os
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--sizes", nargs="+", type=int, default=[8, 64, 256], metavar="MiB")
args = parser.parse_args()
if any(size <= 0 for size in args.sizes):
    parser.error("sizes must be positive")
os.chdir(Path(__file__).resolve().parent.parent)
artifacts = subprocess.check_output(
    ["cargo", "test", "--release", "--no-run", "--message-format=json"], text=True
)
executables = [event["executable"] for line in artifacts.splitlines()
               if (event := json.loads(line)).get("reason") == "compiler-artifact"
               and event.get("executable") and event.get("profile", {}).get("test")]
if len(executables) != 1:
    raise SystemExit(f"Expected one test executable, found {len(executables)}")
for stage in ["encode", "decode"]:
    for size in args.sizes:
        subprocess.run(
            ["/usr/bin/time", "-f", f"{stage} {size} MiB: %M KiB peak RSS, %e s",
             executables[0], "--ignored", "--exact", "streaming::tests::memory_profile"],
            env={**os.environ, "PROFILE_MIB": str(size), "PROFILE_STAGE": stage}, check=True,
        )

#!/usr/bin/env python3
"""Regression: FFmpeg early termination must preserve terminal settings (Unix)."""
import os
from pathlib import Path
import pty
import selectors
import subprocess
import tempfile
import termios
import time

ROOT = Path(__file__).resolve().parent.parent
subprocess.run(["cargo", "build", "--release"], cwd=ROOT, check=True)
BIN = ROOT / "target/release/bytevideo"


def run_in_terminal(arguments, expected_exit=0):
    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    # Explicitly enable the settings this regression used to leave disabled.
    original[3] |= termios.ECHO | termios.ICANON
    termios.tcsetattr(slave, termios.TCSANOW, original)
    captured = bytearray()
    process = None
    try:
        process = subprocess.Popen([str(BIN), *map(str, arguments)], stdin=slave,
                                   stdout=slave, stderr=slave, start_new_session=True)
        deadline = time.monotonic() + 60
        # Drain progress output while the process is running to avoid a full PTY.
        with selectors.DefaultSelector() as selector:
            selector.register(master, selectors.EVENT_READ)
            while process.poll() is None:
                if time.monotonic() > deadline:
                    raise TimeoutError("bytevideo did not finish")
                if selector.select(timeout=0.1):
                    captured.extend(os.read(master, 65536))
            while selector.select(timeout=0):
                captured.extend(os.read(master, 65536))
        assert process.returncode == expected_exit, captured.decode(errors="replace")
        assert termios.tcgetattr(slave) == original, "Terminal settings changed after command"
        return captured.decode(errors="replace")
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        termios.tcsetattr(slave, termios.TCSANOW, original)
        os.close(slave)
        os.close(master)


with tempfile.TemporaryDirectory(prefix="bytevideo-terminal-") as directory:
    work = Path(directory)
    source = work / "input.bin"
    source.write_bytes(b"terminal echo regression")
    # Exercise the default ten-level layout before the binary audio fixture.
    default_video = work / "default.mp4"
    restored = work / "restored.bin"
    run_in_terminal(["encode", source, default_video])
    log = run_in_terminal(["decode", default_video, restored])
    assert "audio skipped" in log, log
    assert restored.read_bytes() == source.read_bytes()
    # Pin two levels: the damage pattern below assumes one packet per frame.
    video = work / "video.mp4"
    run_in_terminal(["encode", source, video, "--gray-levels", "2"])
    log = run_in_terminal(["decode", video, restored])
    assert "16 video frames read; audio skipped" in log, log
    assert restored.read_bytes() == source.read_bytes()
    # Leave 15 shards in the video; compact audio supplies the missing shard.
    audio = work / "needs-audio.mp4"
    subprocess.run([
        "ffmpeg", "-nostdin", "-v", "error", "-i", str(video),
        "-vf", "drawbox=x=0:y=0:w=iw:h=ih:color=black:t=fill:enable='eq(mod(n,24),16)',"
               "select='gte(mod(n,24),8)',setpts=N/(30*TB)",
        "-fps_mode", "vfr", "-c:v", "libx264", "-crf", "28", "-c:a", "copy", str(audio)
    ], check=True)
    log = run_in_terminal(["decode", audio, restored])
    assert "audio used" in log, log
    assert restored.read_bytes() == source.read_bytes()
    # Failed video/audio recovery must also leave the terminal intact.
    missing_output = work / "must-not-exist.bin"
    run_in_terminal(["decode", work / "missing.mp4", missing_output], expected_exit=1)
    assert not missing_output.exists()

print("Terminal settings preserved after encode, video decode, audio repair and failure")

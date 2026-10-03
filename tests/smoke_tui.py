#!/usr/bin/env python3
"""Exercise a real Linux TUI through a pseudo-terminal using disposable files."""
import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import time


def smoke(binary):
    binary = Path(binary).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="storage-analytics-smoke-") as directory:
        work = Path(directory)
        root = work / "fixture"
        nested = root / "nested"
        nested.mkdir(parents=True)
        (nested / "payload.bin").write_bytes(b"x" * 4096)
        (root / "tiny.txt").write_bytes(b"x")
        (root / "loop").symlink_to(root, target_is_directory=True)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
        process = subprocess.Popen([str(binary), str(root)], cwd=work,
            stdin=slave, stdout=slave, stderr=slave,
            env={**os.environ, "TERM": "xterm-256color"})
        os.close(slave)
        output = bytearray()

        def wait_for(text):
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], 0.1)
                if ready:
                    try:
                        output.extend(os.read(master, 65536))
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                        break
                if text.encode() in output:
                    return
                if process.poll() is not None:
                    break
            raise AssertionError(f"TUI did not show {text!r}; exit={process.poll()}; output={bytes(output[-3000:])!r}")

        def wait_for_cache(path, size):
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                with sqlite3.connect(work / "storage_analytics_cache.sqlite3") as cache:
                    row = cache.execute(
                        "SELECT recursive_size FROM directory_cache WHERE display_path = ?", (str(path),)
                    ).fetchone()
                cache.close()
                if row == (size,):
                    return
                if process.poll() is not None:
                    break
                # Drain repaint output while the background scanner finishes.
                if select.select([master], [], [], 0.05)[0]:
                    output.extend(os.read(master, 65536))
            raise AssertionError("Background scan did not update the cached folder size")

        try:
            wait_for("4.00 KB")
            output.clear()
            os.write(master, b"\r")
            wait_for("payload.bin")
            output.clear()
            (nested / "refresh.txt").write_bytes(b"x")
            os.write(master, b"r")
            wait_for("refresh.txt")
            output.clear()
            os.write(master, b"\x7f")
            wait_for("tiny.txt")
            wait_for_cache(nested, 4097)
            os.write(master, b"q")
            assert process.wait(timeout=10) == 0
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            os.close(master)

        assert (nested / "payload.bin").stat().st_size == 4096
        assert (root / "loop").is_symlink()
        with sqlite3.connect(work / "storage_analytics_cache.sqlite3") as cache:
            rows = cache.execute("SELECT display_path, recursive_size FROM directory_cache").fetchall()
        assert any(path == str(nested) and size == 4097 for path, size in rows)
    print("Linux TUI smoke passed: scanning, folder navigation, refresh, cache and quit.")


if __name__ == "__main__":
    smoke(sys.argv[1])

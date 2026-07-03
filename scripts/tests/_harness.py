"""Shared harness for the malkuth Python integration tests.

The wrapped program in CLI tests is `test_app worker`, which reads the
`PORT` env the CLI assigns it and speaks a tiny line protocol:
  ping   -> pong
  health -> port=<P>;gen=<G>;pid=<PID>
  crash  -> (process exits 1)
"""

from __future__ import annotations

import socket
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Optional

ROOT = Path(__file__).resolve().parents[2]  # .../malkuth
TARGET = ROOT / "target" / "debug"
EXAMPLES = TARGET / "examples"


def ensure_built() -> None:
    missing = []
    if not (TARGET / "malkuth").exists():
        missing.append("malkuth")
    if not (EXAMPLES / "test_app").exists():
        missing.append("test_app")
    if missing:
        raise SystemExit(
            f"binaries not built: {missing} — run `just build-bins` first"
        )


def bin_path(name: str) -> Path:
    # Example binaries land under target/debug/examples/
    if name == "test_app":
        return EXAMPLES / name
    return TARGET / name


def free_port() -> int:
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]
    finally:
        s.close()


def wait_port(port: int, host: str = "127.0.0.1", timeout: float = 15.0) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def line_request(port: int, cmd: str, host: str = "127.0.0.1", timeout: float = 5.0) -> str:
    """Send one line, read one line reply."""
    with socket.create_connection((host, port), timeout=timeout) as s:
        s.settimeout(timeout)
        s.sendall((cmd + "\n").encode())
        data = b""
        while not data.endswith(b"\n"):
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
    return data.decode(errors="replace").rstrip("\n")


def parse_kv(line: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for part in line.split(";"):
        if "=" in part:
            k, v = part.split("=", 1)
            out[k.strip()] = v.strip()
    return out


class Proc:
    """A long-running subprocess with merged stdout/stderr captured line-by-line."""

    def __init__(self, args, env: Optional[dict] = None) -> None:
        self.p = subprocess.Popen(
            [str(a) for a in args],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=env,
            bufsize=1,
        )
        self._lines: list[str] = []
        self._lock = threading.Lock()
        self._thr = threading.Thread(target=self._reader, daemon=True)
        self._thr.start()

    def _reader(self) -> None:
        assert self.p.stdout is not None
        for line in self.p.stdout:
            with self._lock:
                self._lines.append(line)

    def output(self) -> str:
        with self._lock:
            return "".join(self._lines)

    def count(self, needle: str) -> int:
        return self.output().count(needle)

    def stop(self) -> None:
        try:
            self.p.terminate()
            self.p.wait(timeout=5)
        except Exception:
            try:
                self.p.kill()
            except Exception:
                pass


# a stable, informative label for a test failure that includes the captured log
def fail_log(proc: "Proc") -> str:
    return "\n----- captured output -----\n" + proc.output() + "\n---------------------------"

def line_request_retry(port: int, cmd: str, timeout: float = 20.0) -> str:
    """Retry line_request; the proxy may accept before any backend is registered."""
    end = time.time() + timeout
    last: Exception = RuntimeError("no attempt")
    while time.time() < end:
        try:
            return line_request(port, cmd, timeout=2.0)
        except OSError as e:
            last = e
            time.sleep(0.2)
    raise last


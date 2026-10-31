"""Shared harness for the malkuth Python integration tests.

The wrapped program in CLI tests is `test_app worker`, which reads the
`PORT` env the CLI assigns it and speaks a tiny line protocol:
  ping   -> pong
  health -> port=<P>;gen=<G>;pid=<PID>
  crash  -> (process exits 1)

# Process hygiene

`Proc` runs every child in **its own process group / session** and `stop()`
signals the whole group (TERM, then KILL), not just the direct child. That
matters because the process under test is a supervisor: killing `malkuth` alone
leaves its `test_app worker` children alive, reparented to init/systemd --user,
where they accumulate (observed: 25 survivors over one afternoon). The group
kill reaps the supervisor and every worker it spawned in one shot.

Two safety nets back that up, registered at import time:

* `atexit` → `teardown()` stops every still-registered `Proc` (an assertion
  failure between spawn and `finally` can no longer leak) and then sweeps;
* `SIGTERM`/`SIGINT` → same teardown before re-raising, so a killed test run
  still cleans up;
* `sweep_orphan_workers()` kills only leftovers of *this* checkout: processes
  whose argv[0] is exactly `target/debug/examples/test_app` **and** whose
  process-group leader is already gone. A live leader means an in-flight run,
  which is never touched — that is what makes the sweep safe to run while other
  tests (or other checkouts) are working.

Limitation: `SIGKILL` of the test process itself cannot run any handler. The
sweep covers that case on the next run.

# Verifying the contract (acceptance note)

Run the suite **alone** (no other malkuth test run in flight), then count
survivors — the number must be zero, and every test must still pass:

```bash
just build-bins
python3 scripts/tests/run_all.py          # or each test_*.py individually
ps -eo args | grep -c 'examples/test_app worker$'   # → 0
```

Before this contract existed the same sequence left 3 survivors from
`test_app_supervise_restart` plus 2 more from `test_app_rolling_update`
(25 accumulated over one afternoon). While a run is live the sweep must be a
no-op — it returns `[]` and does not disturb the run — which is what makes it
safe to call during a concurrent suite.
"""

from __future__ import annotations

import atexit
import os
import signal
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


# ── process hygiene ────────────────────────────────────────────────────────

_POSIX = os.name == "posix"

_ACTIVE: "set[Proc]" = set()
_ACTIVE_LOCK = threading.Lock()


def _kill_group(pgid: int, sig: int) -> None:
    """Signal an entire process group (best effort, never raises)."""
    if pgid <= 0:
        return
    try:
        os.killpg(pgid, sig)
    except (ProcessLookupError, PermissionError, OSError):
        pass


def _kill_tree(pid: int, sig: int) -> None:
    """Windows fallback: `taskkill /T` walks the child tree."""
    try:
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(pid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        pass


def _process_table() -> "list[tuple[int, list[str]]]":
    """(pid, argv) for every readable process; Linux `/proc`, else empty."""
    proc_root = Path("/proc")
    if not proc_root.is_dir():
        return []
    table: "list[tuple[int, list[str]]]" = []
    for entry in proc_root.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            raw = (entry / "cmdline").read_bytes()
        except OSError:
            continue
        argv = [part.decode("utf-8", "replace") for part in raw.split(b"\0") if part]
        if argv:
            table.append((int(entry.name), argv))
    return table


def sweep_orphan_workers() -> "list[int]":
    """Kill leftover `test_app worker` processes owned by this checkout.

    Deliberately narrow, so it is safe to run while other tests or other
    checkouts are working:

    * argv[0] must be exactly THIS checkout's `target/debug/examples/test_app`;
    * argv[1] must be `worker`;
    * no live `malkuth` (this checkout's binary) may exist: with one session per
      `Proc`, an in-flight supervision means live workers, never leftovers.

    Linux-only (`/proc`); returns the pids it signalled.
    """
    if not _POSIX:
        return []
    table = _process_table()
    if not table:
        return []
    target = os.path.realpath(str(EXAMPLES / "test_app"))
    malkuth = os.path.realpath(str(TARGET / "malkuth"))
    # A live malkuth from THIS checkout means a run is in flight: leave it be.
    if any(argv and os.path.realpath(argv[0]) == malkuth for _pid, argv in table):
        return []

    swept: "list[int]" = []
    for pid, argv in table:
        if len(argv) < 2 or argv[1] != "worker":
            continue
        if os.path.realpath(argv[0]) != target:
            continue
        try:
            os.kill(pid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            continue
        swept.append(pid)
    for pid in swept:
        try:  # escalate whatever ignored SIGTERM
            os.kill(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    if swept:
        print(
            f"[harness] swept {len(swept)} orphan test_app worker(s): {swept}",
            file=sys.stderr,
        )
    return swept


def teardown() -> None:
    """Stop every registered child (whole group) and sweep leftovers."""
    with _ACTIVE_LOCK:
        procs = list(_ACTIVE)
    for proc in procs:
        proc.stop()
    sweep_orphan_workers()


def _on_signal(signum, _frame) -> None:
    teardown()
    # Restore the default disposition and re-raise, so the exit status is the
    # one the caller expects.
    try:
        signal.signal(signum, signal.SIG_DFL)
        os.kill(os.getpid(), signum)
    except (OSError, ValueError):
        raise SystemExit(128 + signum)


atexit.register(teardown)
if _POSIX:
    for _sig in (signal.SIGTERM, signal.SIGINT):
        try:
            signal.signal(_sig, _on_signal)
        except (OSError, ValueError):  # not the main thread / unsupported
            pass


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
    """A long-running subprocess with merged stdout/stderr captured line-by-line.

    Every child runs in its **own process group** (`start_new_session` on POSIX,
    `CREATE_NEW_PROCESS_GROUP` on Windows) so [`Proc.stop`] can signal the whole
    tree. That is what keeps a supervised `malkuth` from leaving its
    `test_app worker` children behind.
    """

    def __init__(self, args, env: Optional[dict] = None) -> None:
        kwargs: dict = {}
        if _POSIX:
            kwargs["start_new_session"] = True
        else:  # pragma: no cover - Windows CI is not used for this suite
            kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
        self.p = subprocess.Popen(
            [str(a) for a in args],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=env,
            bufsize=1,
            **kwargs,
        )
        # With one session per child the process group id equals the child pid;
        # capture it now, before the child can be reaped.
        self._pgid = self.p.pid
        self._stopped = False
        with _ACTIVE_LOCK:
            _ACTIVE.add(self)
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
        """Stop the child **and everything it spawned**, then unregister.

        Idempotent. Sends TERM to the whole process group (a supervisor plus its
        workers), waits for the leader, then KILLs the group even if the leader
        already exited — a dead leader is not proof that the workers are gone.
        """
        with _ACTIVE_LOCK:
            if self._stopped:
                _ACTIVE.discard(self)
                return
            self._stopped = True
            _ACTIVE.discard(self)
        if _POSIX:
            _kill_group(self._pgid, signal.SIGTERM)
        else:  # pragma: no cover - Windows CI is not used for this suite
            _kill_tree(self.p.pid, signal.SIGTERM)
            try:
                self.p.terminate()
            except Exception:
                pass
        try:
            self.p.wait(timeout=5)
        except Exception:
            pass
        if _POSIX:
            _kill_group(self._pgid, signal.SIGKILL)
        else:  # pragma: no cover
            _kill_tree(self.p.pid, signal.SIGKILL)
        try:
            self.p.wait(timeout=2)
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


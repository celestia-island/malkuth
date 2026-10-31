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
* `sweep_orphan_workers()` collects leftovers, judged **per worker**: a
  candidate is swept only when it is this checkout's `test_app worker`
  (identity from `/proc/<pid>/exe`, so a leader invoked through a relative path
  or a symlinked `target/` still counts), it has **no live supervising leader**
  in its ancestor chain or process group (`malkuth`, `test_app supervise` and
  `test_app rolling` all count as leaders), and its working directory is inside
  this worktree. A live run is therefore never touched, while a genuine orphan
  is still collected even when another run is in flight.

  `scripts/tests/test_harness_hygiene.py` pins this: three leader shapes each
  "sweep during startup must not kill", a relative-path leader, an orphan
  beside a live run, and an orphan whose leader was SIGKILLed.

Limitation: `SIGKILL` of the test process itself cannot run any handler. What
survives depends on the shape: a leak that leaves a live supervising leader
(e.g. `malkuth`, which `start_new_session` keeps running) is NOT self-healing —
its workers still have a live leader so the sweep must leave them alone, and the
leader is not a sweep candidate at all. That class needs manual cleanup
(`pkill -f <leader> && pkill -f 'examples/test_app worker'`). Leaks whose
leaders are gone are collected on the next run.

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
(25 accumulated over one afternoon). `test_harness_hygiene` runs first in
`run_all.py`; it asserts both halves of the rule — a live run keeps its
workers, and real orphans (including one sitting next to a live run) are
collected.
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
#
# The suite supervises processes, which means killing the direct child is not
# enough: `malkuth` (or `test_app supervise|rolling`) spawns `test_app worker`
# children that survive it and reparent to init / `systemd --user`.
#
# Two mechanisms, both keyed per worker rather than globally:
#   * `Proc.stop()` signals the child's whole process group (TERM -> KILL);
#   * `sweep_orphan_workers()` collects leftovers, but ONLY workers that have
#     no live supervising leader — judged per candidate, never "is anything
#     running somewhere". A live run is therefore never touched, whatever its
#     leader shape or however it was invoked.

_POSIX = os.name == "posix"

# Every child spawned through `Proc` carries this marker (see `Proc.__init__`),
# so a process can be attributed to a run root even when its working directory
# tells us nothing (a child may chdir, and the suite may be launched from
# anywhere). It is the primary worktree dimension; `/proc/<pid>/cwd` is only a
# fallback for processes started before the marker existed.
_RUN_ROOT_ENV = "MALKUTH_HARNESS_ROOT"

_ACTIVE: "set[Proc]" = set()
# RLock: the SIGTERM/SIGINT handler may run teardown while the main thread holds
# this lock, and a non-reentrant lock would deadlock the interpreter.
_ACTIVE_LOCK = threading.RLock()


def _kill_group(pgid: int, sig: int) -> None:
    """Signal an entire process group (best effort, never raises)."""
    if pgid <= 0:
        return
    try:
        os.killpg(pgid, sig)
    except (ProcessLookupError, PermissionError, OSError):
        pass


def _kill_tree(pid: int, sig: int) -> None:
    """Windows fallback: `taskkill /T` walks the child tree.

    Known gap (S3): `taskkill` is always `/F` (hard kill); there is no
    two-phase TERM-then-KILL equivalent here.
    """
    try:
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(pid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        pass


# ── /proc helpers (Linux; the suite's CI lane is Linux) ────────────────────

def _proc_argv(pid: int) -> "list[str]":
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return []
    return [part.decode("utf-8", "replace") for part in raw.split(b"\0") if part]


def _proc_exe(pid: int) -> "Optional[str]":
    """The binary a process actually runs (`/proc/<pid>/exe`).

    Authoritative for identity: argv[0] may be relative (resolved against the
    process's own cwd) or simply a lie, while `exe` is the real file — which is
    what makes the S1-2 case (a leader started as `target/debug/malkuth` from
    its worktree) visible.
    """
    try:
        exe = os.readlink(f"/proc/{pid}/exe")
    except OSError:
        return None
    if exe.endswith(" (deleted)"):
        exe = exe[: -len(" (deleted)")]
    return exe


def _proc_cwd(pid: int) -> "Optional[str]":
    try:
        return os.readlink(f"/proc/{pid}/cwd")
    except OSError:
        return None


def _proc_ppid_pgid(pid: int) -> "Optional[tuple[int, int]]":
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    # "pid (comm) state ppid pgrp ..." — comm may contain spaces/parens, so
    # split after the LAST ')'.
    rparen = stat.rfind(")")
    if rparen < 0:
        return None
    fields = stat[rparen + 2:].split()
    if len(fields) < 3:
        return None
    try:
        return int(fields[1]), int(fields[2])
    except ValueError:
        return None


def _process_table() -> "list[tuple[int, list[str]]]":
    """(pid, argv) for every readable process; Linux `/proc`, else empty."""
    proc_root = Path("/proc")
    if not proc_root.is_dir():
        return []
    table: "list[tuple[int, list[str]]]" = []
    for entry in proc_root.iterdir():
        if not entry.name.isdigit():
            continue
        argv = _proc_argv(int(entry.name))
        if argv:
            table.append((int(entry.name), argv))
    return table


def _checkout_binaries() -> "tuple[str, str]":
    """(malkuth, test_app) real paths for THIS checkout's target dir."""
    return (
        os.path.realpath(str(TARGET / "malkuth")),
        os.path.realpath(str(EXAMPLES / "test_app")),
    )


def _is_supervising_leader(pid: int) -> bool:
    """True when `pid` is a live supervision leader of this checkout.

    Three shapes exist in this suite: `malkuth ... -- <cmd>`,
    `test_app supervise ...` and `test_app rolling ...`. Identity comes from
    `/proc/<pid>/exe` (not argv[0]), so a leader started through a relative
    path or a symlinked `target/` is still recognised.
    """
    exe = _proc_exe(pid)
    if exe is None:
        return False
    malkuth, test_app = _checkout_binaries()
    if exe == malkuth:
        return True
    if exe == test_app:
        argv = _proc_argv(pid)
        return len(argv) > 1 and argv[1] in ("supervise", "rolling")
    return False


def _supervising_leader_of(pid: int) -> "Optional[int]":
    """The live supervising leader `pid` belongs to, or None.

    Walks the ancestor chain (bounded) AND the process-group id, because a
    worker can be spawned directly by its leader or through a short-lived
    intermediary. Returns None when every candidate leader is gone — that is
    exactly the orphan case the sweep exists for.
    """
    seen: "set[int]" = set()
    chain: "list[int]" = []
    cur = pid
    for _ in range(6):
        info = _proc_ppid_pgid(cur)
        if info is None:
            break
        ppid, pgid = info
        if pgid > 0 and pgid not in seen and pgid != pid:
            seen.add(pgid)
            chain.append(pgid)
        if ppid <= 0:
            break
        if ppid not in seen:
            seen.add(ppid)
            chain.append(ppid)
        cur = ppid
    for candidate in chain:
        if _is_supervising_leader(candidate):
            return candidate
    return None


def _same_path(a: str, b: str) -> bool:
    """Identity of two paths by inode, not by string.

    `samefile` compares st_dev/st_ino, which is what makes the bind-mounted
    spellings of one directory (`/mnt/codespace/...` and `/mnt/work/codespace/...`
    on this host) compare equal — a string-prefix test would call them
    different. Falls back to `realpath` equality when a path no longer exists.
    """
    try:
        return os.path.samefile(a, b)
    except OSError:
        return os.path.realpath(a) == os.path.realpath(b)


def _within_root(path: "Optional[str]") -> bool:
    """Whether `path` is this worktree or sits inside it (by inode, not prefix)."""
    if not path:
        return False
    current = path
    for _ in range(64):
        if _same_path(current, str(ROOT)):
            return True
        parent = os.path.dirname(current)
        if parent == current:
            return False
        current = parent
    return False


def _proc_environ(pid: int) -> "dict[str, str]":
    try:
        raw = Path(f"/proc/{pid}/environ").read_bytes()
    except OSError:
        return {}
    env: "dict[str, str]" = {}
    for entry in raw.split(b"\0"):
        if not entry:
            continue
        key, sep, value = entry.partition(b"=")
        if sep:
            env[key.decode("utf-8", "replace")] = value.decode("utf-8", "replace")
    return env


def _has_run_root_marker(pid: int) -> bool:
    """Whether `pid` was spawned by a `Proc` of THIS worktree's run."""
    marker = _proc_environ(pid).get(_RUN_ROOT_ENV)
    if not marker:
        return False
    return _same_path(marker, str(ROOT)) or _within_root(marker)


def _belongs_to_checkout(pid: int) -> bool:
    """Worktree dimension for one process.

    Primary: the run-root marker injected by `Proc` (survives a child that
    chdir's, and works when the suite was launched from any directory).
    Fallback: the working directory is inside this worktree — compared by inode
    so the bind-mounted path spellings of one directory both count as "inside".
    """
    if _has_run_root_marker(pid):
        return True
    return _within_root(_proc_cwd(pid))


def _is_our_worker(pid: int, argv: "list[str]") -> bool:
    """Identity of a worker: this checkout's `test_app` binary running `worker`."""
    if len(argv) < 2 or argv[1] != "worker":
        return False
    _malkuth, test_app = _checkout_binaries()
    return _proc_exe(pid) == test_app


def live_worker_pids() -> "list[int]":
    """pids of this checkout's `test_app worker` processes, live or left over.

    Uses the same identity + worktree predicates as `sweep_orphan_workers`, so
    "what the sweep would consider" and "what the tests consider" cannot drift.
    """
    if not _POSIX or not Path("/proc").is_dir():
        return []
    return [
        pid
        for pid, argv in _process_table()
        if _is_our_worker(pid, argv) and _belongs_to_checkout(pid)
    ]


def sweep_orphan_workers() -> "list[int]":
    """Kill leftover `test_app worker` processes owned by this checkout.

    Judged **per worker**, so a live run is never collateral damage:

    * the candidate must be this checkout's `test_app worker` (identity from
      `/proc/<pid>/exe`, not argv[0]);
    * it must have **no live supervising leader** — neither in its ancestor
      chain nor as its process-group leader — covering `malkuth` and
      `test_app supervise|rolling` leaders alike;
    * it must belong to this worktree — either by the run-root marker every
      `Proc` child carries, or (fallback) by a working directory inside it,
      compared by inode so bind-mounted path spellings still count. The shared
      `target/` cache and inherited working directories mean the binary path
      alone cannot identify a worktree.

    Anything that fails one of those checks is left alone; a worker whose leader
    is gone is by definition a leftover (the L57 leak) and is swept.

    Linux-only (`/proc`); returns the pids it signalled.
    """
    if not _POSIX or not Path("/proc").is_dir():
        return []
    swept: "list[int]" = []
    for pid, argv in _process_table():
        if not _is_our_worker(pid, argv):
            continue
        if _supervising_leader_of(pid) is not None:
            continue  # in-flight supervision: never touch
        if not _belongs_to_checkout(pid):
            continue  # another worktree sharing the same target dir
        try:
            os.kill(pid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            continue
        swept.append(pid)
    # No grace window: these workers are already-orphaned leftovers, so a
    # TERM-then-KILL double tap just guarantees they cannot linger.
    for pid in swept:
        try:
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
    try:
        teardown()
    except Exception:  # never mask the signal with teardown noise
        pass
    # Restore the default disposition and re-raise, so the exit status is the
    # one the caller expects.
    try:
        signal.signal(signum, signal.SIG_DFL)
        os.kill(os.getpid(), signum)
    except (OSError, ValueError):
        raise SystemExit(128 + signum)


# Registered at import: both the normal exit (atexit) and a terminated run
# (SIGTERM/SIGINT) clean up. SIGKILL of the test process itself cannot run any
# handler — the sweep covers that case on the next run.
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

    `cwd` is forwarded to the child; the hygiene tests use it to pin the
    working directory the sweep's worktree check keys on.
    """

    def __init__(self, args, env: Optional[dict] = None, cwd: Optional[str] = None) -> None:
        kwargs: dict = {}
        if _POSIX:
            kwargs["start_new_session"] = True
        else:  # pragma: no cover - Windows CI is not used for this suite
            kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
        # Default the child's working directory to the worktree: the suite may
        # be launched from anywhere, and a worker's cwd is the fallback the
        # sweep uses to attribute it. The marker below is the primary
        # attribution and survives a child that chdir's.
        if cwd is None:
            cwd = str(ROOT)
        run_env = dict(env) if env is not None else dict(os.environ)
        run_env.setdefault(_RUN_ROOT_ENV, str(ROOT))
        self.p = subprocess.Popen(
            [str(a) for a in args],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=run_env,
            bufsize=1,
            cwd=cwd,
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


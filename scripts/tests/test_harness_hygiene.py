"""Harness hygiene: the orphan sweep must never touch a live run, and must
still collect real leftovers.

Every case here is a regression pin for the failure shape L57 describes: the
suite supervises processes, killing only the direct child leaves workers behind,
and a sweep that decides "is anything running?" globally either leaks (when it
is too eager to skip) or kills live work (when it is not precise enough).

The three leader shapes this suite actually uses are exercised:
  * `malkuth ... -- test_app worker`           (CLI proxy/rolling tests)
  * `test_app supervise --pods N ...`          (supervise restart test)
  * `test_app rolling --pods N ...`            (rolling update test)
"""
import os
import signal
import sys
import time
import pathlib
import tempfile

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import (  # noqa: E402
    ROOT,
    Proc,
    bin_path,
    free_port,
    line_request,
    line_request_retry,
    live_worker_pids,
    parse_kv,
    sweep_orphan_workers,
    wait_port,
)


def _wait_worker_pids(at_least: int, timeout: float = 25.0) -> list[int]:
    """Wait until at least `at_least` worker processes exist; return their pids."""
    end = time.time() + timeout
    while time.time() < end:
        pids = live_worker_pids()
        if len(pids) >= at_least:
            return pids
        time.sleep(0.1)
    raise AssertionError(f"expected >= {at_least} test_app worker(s), got {live_worker_pids()}")


def _sweep_until(condition, timeout: float = 25.0, interval: float = 0.15) -> list[int]:
    """Call the sweep repeatedly until `condition()` holds; return every pid it
    claimed (empty is the only acceptable answer for a live run).

    Sweeping *during* startup is deliberate: that is the window in which the
    pre-fix sweep killed live workers (R2: `gen0 not up`, `SWEPT [...]` while the
    leader was alive the whole time).
    """
    claimed: list[int] = []
    end = time.time() + timeout
    while time.time() < end:
        claimed.extend(sweep_orphan_workers())
        if condition():
            return claimed
        time.sleep(interval)
    raise AssertionError(f"condition never held within {timeout}s; swept={claimed}")


def _sweep_settled(rounds: int = 3, pause: float = 0.25) -> list[int]:
    """Sweep a few more times once the run is definitively live.

    `_sweep_until` may return in the same iteration in which the readiness
    condition first holds, i.e. before any worker existed. The mutation guard
    needs sweeps that definitely see live workers, or it can pass by timing
    instead of by correctness.
    """
    claimed: list[int] = []
    for _ in range(rounds):
        claimed.extend(sweep_orphan_workers())
        time.sleep(pause)
    return claimed


def test_sweep_skips_live_malkuth_run() -> None:
    """A live `malkuth` supervision keeps its workers: sweep returns nothing."""
    pub = free_port()
    cli = Proc([
        bin_path("malkuth"),
        "--pod-count", "2",
        "--proxy", f"{pub}:{pub}-{pub + 10}",
        "--", bin_path("test_app"), "worker",
    ], cwd=str(ROOT))
    try:
        claimed = _sweep_until(lambda: wait_port(pub, timeout=0.2))
        before = _wait_worker_pids(2)
        claimed += _sweep_settled()
        assert claimed == [], f"sweep killed a live malkuth run's workers: {claimed}"
        assert line_request_retry(pub, "ping") == "pong", "live run was disturbed"
        assert claimed == [] and sweep_orphan_workers() == [], "sweep must stay empty"
        assert sorted(live_worker_pids()) == sorted(before), "live workers disappeared"
    finally:
        cli.stop()


def test_sweep_skips_live_test_app_supervise() -> None:
    """A live `test_app supervise` (a leader whose argv says nothing about
    malkuth) keeps its workers."""
    base = free_port()
    app = Proc([
        bin_path("test_app"), "supervise",
        "--pods", "2", "--port-base", str(base),
    ], cwd=str(ROOT))
    try:
        claimed = _sweep_until(lambda: wait_port(base + 1, timeout=0.2))
        before = _wait_worker_pids(2)
        claimed += _sweep_settled()
        assert claimed == [], f"sweep killed a live supervise leader's workers: {claimed}"
        pid = int(parse_kv(line_request_retry(base + 1, "health"))["pid"])
        assert pid in before, f"health reports {pid}, live workers are {before}"
        assert sorted(live_worker_pids()) == sorted(before), "live workers disappeared"
    finally:
        app.stop()


def test_sweep_skips_live_test_app_rolling() -> None:
    """A live `test_app rolling` leader keeps its workers and still completes
    (the R2 case that died with `gen0 not up`)."""
    base = free_port()
    app = Proc([
        bin_path("test_app"), "rolling",
        "--pods", "2", "--port-base", str(base),
    ], cwd=str(ROOT))
    try:
        claimed = _sweep_until(
            lambda: wait_port(base + 1, timeout=0.2) and wait_port(base + 2, timeout=0.2)
        )
        claimed += _sweep_settled()
        assert claimed == [], f"sweep killed a live rolling leader's workers: {claimed}"

        done = False
        for _ in range(120):
            if "ROLLING_DONE" in app.output():
                done = True
                break
            time.sleep(0.25)
        assert done, "rolling did not complete after the sweep attempt" + ("\n" + app.output())
        assert parse_kv(line_request_retry(base + 3, "health"))["gen"] == "1"
    finally:
        app.stop()


def test_sweep_skips_relative_path_leader() -> None:
    """A leader started through a RELATIVE path from the worktree must still be
    recognised (argv[0] resolution must not use the sweeper's own cwd)."""
    pub = free_port()
    rel_malkuth = os.path.relpath(str(bin_path("malkuth")), str(ROOT))
    cli = Proc([
        rel_malkuth,
        "--pod-count", "1",
        "--proxy", f"{pub}:{pub}-{pub + 10}",
        "--", bin_path("test_app"), "worker",
    ], cwd=str(ROOT))
    elsewhere = tempfile.mkdtemp(prefix="malkuth_sweep_cwd_")
    cwd_before = os.getcwd()
    try:
        assert wait_port(pub, timeout=25), "proxy did not come up" + ("\n" + cli.output())
        before = _wait_worker_pids(1)
        # Sweep from OUTSIDE the worktree: a sweep that resolves the leader's
        # relative argv[0] against its own cwd cannot see this leader (S1-2),
        # so it would kill live workers. Identity must come from /proc/<pid>/exe.
        os.chdir(elsewhere)
        try:
            claimed = _sweep_settled()
        finally:
            os.chdir(cwd_before)
        assert claimed == [], f"sweep must recognise a relative-path leader, killed {claimed}"
        assert sorted(live_worker_pids()) == sorted(before), "live workers disappeared"
        assert line_request_retry(pub, "ping") == "pong", "live run was disturbed"
    finally:
        os.chdir(cwd_before)
        cli.stop()


def test_sweep_collects_orphans_beside_a_live_run() -> None:
    """A live run and an orphan can coexist: the sweep must collect the orphan
    and leave the live workers alone.

    This is the case a global "is anything running?" guard gets wrong in the
    other direction: it would skip *everything* (leaking the orphan) the moment
    any run is in flight.
    """
    pub = free_port()
    live = Proc([
        bin_path("malkuth"), "--pod-count", "1",
        "--proxy", f"{pub}:{pub}-{pub + 10}",
        "--", bin_path("test_app"), "worker",
    ], cwd=str(ROOT))
    doomed = None
    try:
        assert wait_port(pub, timeout=25), "live run did not come up" + ("\n" + live.output())
        live_pids = set(_wait_worker_pids(1))

        doomed = Proc([
            bin_path("test_app"), "supervise",
            "--pods", "1", "--port-base", str(free_port()),
        ], cwd=str(ROOT))
        # Wait until the second run's worker exists, then orphan it.
        end = time.time() + 25
        while time.time() < end:
            if len(set(live_worker_pids()) - live_pids) >= 1:
                break
            time.sleep(0.1)
        else:
            raise AssertionError("the doomed run never spawned a worker" + ("\n" + doomed.output()))
        os.kill(doomed.p.pid, signal.SIGKILL)
        doomed.p.wait(timeout=10)
        time.sleep(0.5)
        orphan_pids = sorted(set(live_worker_pids()) - live_pids)
        assert orphan_pids, "the doomed run left no orphan to test with"

        swept = sweep_orphan_workers()
        assert sorted(swept) == orphan_pids, (
            f"sweep collected {swept}, wanted the orphan(s) {orphan_pids}"
        )
        assert sorted(live_worker_pids()) == sorted(live_pids), "live workers disappeared"
        assert line_request_retry(pub, "ping") == "pong", "live run was disturbed"
    finally:
        if doomed is not None:
            doomed.stop()
        live.stop()


def test_sweep_collects_orphans_after_leader_sigkill() -> None:
    """The actual L57 shape: SIGKILL the leader, and the orphaned workers are
    swept (a dead leader is not proof the workers died)."""
    base = free_port()
    app = Proc([
        bin_path("test_app"), "supervise",
        "--pods", "2", "--port-base", str(base),
    ], cwd=str(ROOT))
    try:
        assert wait_port(base + 1, timeout=25), "worker 0 did not come up"
        workers = _wait_worker_pids(2)
        # SIGKILL the leader: no handler, no teardown — the workers reparent.
        os.kill(app.p.pid, signal.SIGKILL)
        app.p.wait(timeout=10)
        time.sleep(0.5)
        survivors = [pid for pid in workers if pathlib.Path(f"/proc/{pid}").exists()]
        assert survivors == workers, f"workers died with the leader: {workers} -> {survivors}"
        swept = sweep_orphan_workers()
        assert sorted(swept) == sorted(workers), f"sweep collected {swept}, wanted {workers}"
        time.sleep(0.3)
        still = [pid for pid in workers if pathlib.Path(f"/proc/{pid}").exists()]
        assert still == [], f"orphans survived the sweep: {still}"
    finally:
        app.stop()


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            fn()
            print(f"{name}: PASS")

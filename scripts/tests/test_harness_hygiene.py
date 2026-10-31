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
    _supervising_leader_of,
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


# The starting state is not assumed clean: other sessions on this host may own
# workers, and a leftover can appear at any moment (concurrent suite runs are
# normal here). So no assertion may say "the sweep returned nothing" — it says
# "the sweep did not kill anything that was ALREADY alive before that call".
# A foreign orphan that shows up mid-window is a legitimate collect, not a red.
_PREEXISTING: "set[int]" = set()


def _preclean() -> None:
    """Start from a clean slate when we can: collect leftovers from earlier runs."""
    sweep_orphan_workers()
    _PREEXISTING.update(live_worker_pids())  # a concurrent run's live workers


_preclean()


def _wait_worker_pids(at_least: int, timeout: float = 25.0) -> list[int]:
    """Wait until at least `at_least` worker processes exist; return their pids."""
    end = time.time() + timeout
    while time.time() < end:
        pids = live_worker_pids()
        if len(pids) >= at_least:
            return pids
        time.sleep(0.1)
    raise AssertionError(f"expected >= {at_least} test_app worker(s), got {live_worker_pids()}")


def _workers_under(leader_pids: "set[int]") -> "set[int]":
    """Live workers whose supervising leader is one of `leader_pids`.

    This is what "already alive" must mean for the assertions: workers of the
    run under test. A foreign orphan (a concurrent session's leftover) is not
    ours to judge — if a sweep collects it, that is correct behaviour, not a
    false red.
    """
    owned: "set[int]" = set()
    for pid in live_worker_pids():
        if _supervising_leader_of(pid) in leader_pids:
            owned.add(pid)
    return owned


def _sweep_rounds(condition=None, owned=None, timeout: float = 25.0, interval: float = 0.15,
                  settled: int = 3, pause: float = 0.25):
    """Sweep repeatedly, recording `(alive_before_this_call, claimed)` per call.

    `condition` (if given) gates the startup window: sweep while the run is
    coming up, then take `settled` extra rounds so the call set definitely
    includes sweeps taken while the live workers exist.
    """
    def before_now() -> "set[int]":
        return set(owned()) if owned is not None else set()

    calls: "list[tuple[set[int], list[int]]]" = []
    end = time.time() + timeout
    while True:
        before = before_now()
        claimed = sweep_orphan_workers()
        calls.append((before, claimed))
        if condition is None:
            if len(calls) >= settled:
                return calls
            time.sleep(pause)
            continue
        if condition():
            for _ in range(settled):
                b = before_now()
                c = sweep_orphan_workers()
                calls.append((b, c))
                time.sleep(pause)
            return calls
        if time.time() >= end:
            raise AssertionError(f"condition never held within {timeout}s (sweeps: {calls})")
        time.sleep(interval)


def _killed_that_were_alive(calls) -> "list[int]":
    """Pids the sweep claimed although they were already alive when the call started."""
    bad: "list[int]" = []
    for before, claimed in calls:
        bad.extend(sorted(set(claimed) & before))
    return sorted(set(bad))


def _assert_no_live_kills(calls, context: str) -> None:
    killed = _killed_that_were_alive(calls)
    assert killed == [], f"{context}: sweep killed worker(s) that were already alive: {killed}"


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
        calls = _sweep_rounds(lambda: wait_port(pub, timeout=0.2),
                              owned=lambda: _workers_under({cli.p.pid}))
        before = _wait_worker_pids(2)
        _assert_no_live_kills(calls, "live malkuth run")
        assert line_request_retry(pub, "ping") == "pong", "live run was disturbed"
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
        calls = _sweep_rounds(lambda: wait_port(base + 1, timeout=0.2),
                              owned=lambda: _workers_under({app.p.pid}))
        before = _wait_worker_pids(2)
        _assert_no_live_kills(calls, "live supervise leader")
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
        calls = _sweep_rounds(
            lambda: wait_port(base + 1, timeout=0.2) and wait_port(base + 2, timeout=0.2),
            owned=lambda: _workers_under({app.p.pid}),
        )
        _assert_no_live_kills(calls, "live rolling leader")

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
            calls = _sweep_rounds(None, owned=lambda: _workers_under({cli.p.pid}))
        finally:
            os.chdir(cwd_before)
        _assert_no_live_kills(calls, "relative-path leader")
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


def test_external_orphan_midrun_is_collected_without_false_red() -> None:
    """An orphan that appears AFTER import must be collected without being
    reported as "sweep killed a live worker".

    This is the R3 false red: the module-level baseline can only know about
    leftovers that existed at import time, so the assertions must judge
    "workers of THIS run", not "all workers that exist". A foreign orphan is a
    legitimate collect.
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
        own = _workers_under({live.p.pid})
        assert own, "the live run has no worker"

        # A second supervision appears now (i.e. after import) and is orphaned.
        doomed = Proc([
            bin_path("test_app"), "supervise",
            "--pods", "1", "--port-base", str(free_port()),
        ], cwd=str(ROOT))
        end = time.time() + 25
        while time.time() < end:
            if _workers_under({doomed.p.pid}):
                break
            time.sleep(0.1)
        else:
            raise AssertionError("the doomed run never spawned a worker")
        os.kill(doomed.p.pid, signal.SIGKILL)
        doomed.p.wait(timeout=10)
        time.sleep(0.5)
        orphan = set(live_worker_pids()) - own
        assert orphan, "no external orphan appeared"

        calls = _sweep_rounds(None, owned=lambda: _workers_under({live.p.pid}), settled=4)
        _assert_no_live_kills(calls, "live run while an external orphan is collected")
        assert not (orphan & set(live_worker_pids())), "the external orphan was not collected"
        assert line_request_retry(pub, "ping") == "pong", "live run disturbed"
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
        assert set(workers) <= set(swept), f"sweep collected {swept}, wanted {workers} included"
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

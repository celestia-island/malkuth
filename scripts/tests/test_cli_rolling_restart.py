"""CLI: a file change under --watch triggers a rolling restart of the pods."""
import os
import sys
import time
import tempfile
import pathlib
sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import Proc, bin_path, free_port, wait_port  # noqa: E402


def test_cli_rolling_restart() -> None:
    pub = free_port()
    watched = tempfile.mkdtemp(prefix="malkuth_watch_")
    seed = os.path.join(watched, "src.txt")
    with open(seed, "w") as f:
        f.write("v0\n")

    cli = Proc([
        bin_path("malkuth"),
        "--watch", watched,
        "--pod-count", "2",
        "--proxy", f"{pub}:{pub}-{pub + 10}",
        "--", bin_path("test_app"), "worker",
    ])
    try:
        assert wait_port(pub, timeout=25), "proxy did not come up"
        ready_before = cli.count("WORKER_READY")
        assert ready_before >= 2, f"expected >=2 initial workers, got {ready_before}"

        time.sleep(1.0)  # let the watcher settle
        with open(seed, "a") as f:  # trigger a change → rolling restart
            f.write("v1\n")
        time.sleep(3.0)  # 400ms debounce + restart

        ready_after = cli.count("WORKER_READY")
        assert ready_after > ready_before, (
            f"no restart detected on file change ({ready_before} -> {ready_after})"
            + ("\n" + cli.output())
        )
    finally:
        cli.stop()


if __name__ == "__main__":
    test_cli_rolling_restart()
    print("test_cli_rolling_restart: PASS")

"""CLI: a file change under --watch triggers a rolling restart of the pods."""
import os
import sys
import time
import tempfile
import pathlib
sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import Proc, bin_path, free_port, line_request_retry, parse_kv, wait_port  # noqa: E402


def test_cli_rolling_restart() -> None:
    pub = free_port()
    watched = tempfile.mkdtemp(prefix="malkuth_watch_")
    seed = os.path.join(watched, "src.txt")
    with open(seed, "w") as f:
        f.write("v0\n")

    cli = Proc([
        bin_path("malkuth"),
        "--watch", watched,
        "--debounce", "1",
        "--pod-count", "1",
        "--proxy", f"{pub}:{pub}-{pub + 10}",
        "--", bin_path("test_app"), "worker",
    ])
    try:
        assert wait_port(pub, timeout=25), "proxy did not come up"
        pid_before = int(parse_kv(line_request_retry(pub, "health"))["pid"])

        time.sleep(1.0)  # let the watcher settle
        with open(seed, "a") as f:  # trigger a change → rolling restart
            f.write("v1\n")

        pid_after = pid_before
        for _ in range(80):
            time.sleep(0.25)
            try:
                pid_after = int(parse_kv(line_request_retry(pub, "health", timeout=2.0))["pid"])
            except Exception:
                continue
            if pid_after != pid_before:
                break
        assert pid_after != pid_before, (
            f"no restart detected on file change (pid {pid_before} -> {pid_after})"
            + ("\n" + cli.output())
        )
    finally:
        cli.stop()


if __name__ == "__main__":
    test_cli_rolling_restart()
    print("test_cli_rolling_restart: PASS")

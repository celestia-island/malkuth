"""CLI: replacing the supervised binary itself triggers a rolling restart even
when the build command produces no changes to the watched paths.

Regression test for the production bug where a freshly deployed binary
(e.g. /usr/local/bin/arona) was silently not loaded because the vite build
output was unchanged and malkuth logged
`build produced no changes, skipping restart`.
"""
import os
import shutil
import sys
import time
import tempfile
import pathlib
sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import Proc, bin_path, free_port, line_request_retry, parse_kv, wait_port  # noqa: E402


def test_cli_binary_change_restarts() -> None:
    pub = free_port()
    watched = tempfile.mkdtemp(prefix="malkuth_binwatch_")
    bin_src = bin_path("test_app")
    bin_copy = os.path.join(watched, "test_app")
    shutil.copy2(bin_src, bin_copy)
    os.chmod(bin_copy, 0o755)

    cli = Proc([
        bin_path("malkuth"),
        "--watch", watched,
        "--build", "true",  # no-op: never touches the watched paths
        "--debounce", "1",
        "--pod-count", "1",
        "--proxy", f"{pub}:{pub}-{pub + 10}",
        "--", bin_copy, "worker",
    ])
    try:
        assert wait_port(pub, timeout=25), "proxy did not come up"
        pid_before = int(parse_kv(line_request_retry(pub, "health"))["pid"])

        time.sleep(1.0)  # let the watcher settle
        # Replace the supervised binary. Write a staged copy then atomically
        # rename it over the running one (a running executable cannot be
        # opened for writing: ETXTBSY). copy2 preserves the source mtime, so
        # the mtime snapshot is unchanged — only the binary-change detection
        # can trigger the restart here.
        staged = bin_copy + ".new"
        shutil.copy2(bin_src, staged)
        os.replace(staged, bin_copy)
        time.sleep(3.0)  # 1s trailing-edge debounce + build + restart
        assert "supervised binary changed" in cli.output(), (
            "expected 'supervised binary changed' log line" + ("\n" + cli.output())
        )

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
            f"no restart after supervised binary change with --build true "
            f"(pid {pid_before} -> {pid_after})"
            + ("\n" + cli.output())
        )
    finally:
        cli.stop()


if __name__ == "__main__":
    test_cli_binary_change_restarts()
    print("test_cli_binary_change_restarts: PASS")

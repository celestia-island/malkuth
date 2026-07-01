"""API: the test-app supervises copies of itself and restarts a killed worker."""
import os, signal, sys, time, pathlib
sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import Proc, bin_path, free_port, line_request, parse_kv, wait_port  # noqa: E402


def test_app_supervise_restart() -> None:
    base = free_port()
    app = Proc([
        bin_path("test_app"), "supervise",
        "--pods", "3", "--port-base", str(base),
    ])
    try:
        assert wait_port(base + 1, timeout=25), "worker 0 did not come up"
        pid0 = int(parse_kv(line_request(base + 1, "health"))["pid"])

        # kill the worker; the Supervisor (Permanent policy) must respawn it
        os.kill(pid0, signal.SIGTERM)

        pid1 = None
        for _ in range(60):
            time.sleep(0.25)
            try:
                pid1 = int(parse_kv(line_request(base + 1, "health"))["pid"])
            except Exception:
                continue
            if pid1 and pid1 != pid0:
                break
        assert pid1 and pid1 != pid0, f"worker not restarted ({pid0} -> {pid1})" + ("\n" + app.output())
    finally:
        app.stop()


if __name__ == "__main__":
    test_app_supervise_restart()
    print("test_app_supervise_restart: PASS")

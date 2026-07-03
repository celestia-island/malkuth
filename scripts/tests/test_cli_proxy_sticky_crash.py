"""CLI: same client IP sticks to one backend; crashing it is tolerated
(either re-routed to another pod, or the crashed pod is restarted)."""
import sys, time, pathlib
sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import Proc, bin_path, free_port, line_request, line_request_retry, parse_kv, wait_port  # noqa: E402


def test_cli_proxy_sticky_and_crash() -> None:
    pub = free_port()
    cli = Proc([
        bin_path("malkuth"),
        "--pod-count", "3",
        "--proxy", f"{pub}:{pub}-{pub + 10}",
        "--", bin_path("test_app"), "worker",
    ])
    try:
        assert wait_port(pub, timeout=25), "proxy did not come up"

        # sticky: repeated requests from the same client (127.0.0.1) hit one backend
        first = parse_kv(line_request_retry(pub, "health"))
        backend, pid0 = first["port"], first["pid"]
        for _ in range(4):
            again = parse_kv(line_request_retry(pub, "health"))
            assert again["port"] == backend, f"sticky broken: {backend} vs {again['port']}"

        # crash that backend through the proxy (connection drops)
        try:
            line_request(pub, "crash")
        except OSError:
            pass

        # service must recover — either re-routed to another pod (port differs)
        # or the crashed pod respawned (pid differs).
        recovered = False
        for _ in range(80):
            time.sleep(0.2)
            try:
                a = parse_kv(line_request_retry(pub, "health", timeout=3.0))
            except Exception:
                continue
            if a.get("port") != backend or a.get("pid") != pid0:
                recovered = True
                break
        assert recovered, "service did not recover after crash" + ("\n" + cli.output())
    finally:
        cli.stop()


if __name__ == "__main__":
    test_cli_proxy_sticky_and_crash()
    print("test_cli_proxy_sticky_and_crash: PASS")

"""CLI: same client IP sticks to one backend; crashing it re-routes."""
import sys, time, pathlib
sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import Proc, bin_path, free_port, line_request, line_request_retry, parse_kv, wait_port  # noqa: E402


def test_cli_proxy_sticky_and_crash() -> None:
    pub = free_port()
    cli = Proc([
        bin_path("malkuth"),
        "--pod-count", "3",
        "--proxy", f"{pub}:{pub}-{pub + 10}",
        "--", bin_path("malkuth-test-app"), "worker",
    ])
    try:
        assert wait_port(pub, timeout=25), "proxy did not come up"

        # sticky: repeated requests from the same client (127.0.0.1) hit one backend
        first = parse_kv(line_request_retry(pub, "health"))
        backend = first["port"]
        for _ in range(4):
            again = parse_kv(line_request(pub, "health"))
            assert again["port"] == backend, f"sticky broken: {backend} vs {again['port']}"

        # crash that backend through the proxy (connection drops)
        try:
            line_request(pub, "crash")
        except OSError:
            pass
        time.sleep(2.5)  # CLI detects exit, removes the dead pod from the pool

        # next request must land on a DIFFERENT backend
        after = parse_kv(line_request_retry(pub, "health"))
        assert after["port"] != backend, (
            f"did not re-route after crash ({backend}=={after['port']})" + ("\n" + cli.output())
        )
    finally:
        cli.stop()


if __name__ == "__main__":
    test_cli_proxy_sticky_and_crash()
    print("test_cli_proxy_sticky_and_crash: PASS")

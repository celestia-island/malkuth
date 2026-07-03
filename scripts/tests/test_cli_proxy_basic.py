"""CLI: a single pod behind the sticky proxy answers via the public port."""
import sys, pathlib
sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import Proc, bin_path, free_port, line_request, line_request_retry, wait_port  # noqa: E402


def test_cli_proxy_basic() -> None:
    pub = free_port()
    cli = Proc([
        bin_path("malkuth"),
        "--pod-count", "1",
        "--proxy", f"{pub}:{pub}-{pub + 10}",
        "--", bin_path("test_app"), "worker",
    ])
    try:
        assert wait_port(pub, timeout=25), "proxy did not come up" + ("\n" + cli.output())
        assert line_request_retry(pub, "ping") == "pong", "ping did not return pong"
        h = line_request_retry(pub, "health")
        assert h.startswith("port=") and "gen=0" in h, f"bad health reply: {h!r}"
    finally:
        cli.stop()


if __name__ == "__main__":
    test_cli_proxy_basic()
    print("test_cli_proxy_basic: PASS")

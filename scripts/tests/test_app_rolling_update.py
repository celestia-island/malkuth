"""API: the test-app performs a gradual gray update gen-0 -> gen-1."""
import sys, time, pathlib
sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import Proc, bin_path, free_port, line_request, parse_kv, wait_port  # noqa: E402


def test_app_rolling_update() -> None:
    base = free_port()
    app = Proc([
        bin_path("test_app"), "rolling",
        "--pods", "2", "--port-base", str(base),
    ])
    try:
        # gen-0 comes up on base+1..base+2
        assert wait_port(base + 1, timeout=25) and wait_port(base + 2, timeout=25), "gen0 not up"
        assert parse_kv(line_request(base + 1, "health"))["gen"] == "0"

        # wait for the gradual rolling to complete
        done = False
        for _ in range(80):
            time.sleep(0.25)
            if "ROLLING_DONE" in app.output():
                done = True
                break
        assert done, "rolling did not report ROLLING_DONE" + ("\n" + app.output())

        # gen-1 now serving on base+3..base+4; gen-0 drained (closed)
        assert parse_kv(line_request(base + 3, "health"))["gen"] == "1"
        assert parse_kv(line_request(base + 4, "health"))["gen"] == "1"
        assert not wait_port(base + 1, timeout=1.0), "gen0 port still open after drain"
    finally:
        app.stop()


if __name__ == "__main__":
    test_app_rolling_update()
    print("test_app_rolling_update: PASS")

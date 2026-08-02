"""CLI: the `--serve` front door tunnels WebSocket upgrades to the backend.

The shittim-chest webui chat uses `GET /api/rpc?workspace=...` over WebSocket,
so a working front-door proxy must handle the upgrade handshake (reqwest
cannot). The echo backend is `test_app ws-echo` (raw WS, no deps).
"""
import pathlib
import socket
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from _harness import Proc, bin_path, free_port, wait_port  # noqa: E402

import websocket  # websocket-client


def _wait_backend(port: int, timeout: float = 15.0) -> None:
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"backend on {port} never came up")


def _front(args: list) -> Proc:
    """Start malkuth in serve mode pointed at the ws-echo backend."""
    return Proc([
        bin_path("malkuth"),
        "--info-port", str(args[0]),
        "--serve", f"http://127.0.0.1:{args[1]}",
        "--serve-host", f"127.0.0.1:{args[0]}",
        "--", "sleep", "300",
    ])


def _echo_backend(port: int) -> Proc:
    return Proc([bin_path("test_app"), "ws-echo"], env={"PORT": str(port)})


def test_cli_proxy_websocket_echo() -> None:
    app_port = free_port()
    info_port = free_port()
    cli = _front([info_port, app_port])
    app = _echo_backend(app_port)
    try:
        assert wait_port(info_port, timeout=25), "front door did not come up" + cli.output()
        _wait_backend(app_port)

        # The real shittim-chest chat path: WS handshake to /api/rpc with the
        # nonce cookie, Host matching --serve-host.
        ws = websocket.create_connection(
            f"ws://127.0.0.1:{info_port}/api/rpc?workspace=test",
            cookie="__malkuth_nonce=1",
            timeout=5,
        )
        try:
            ws.send("hello via proxy")
            assert ws.recv() == "hello via proxy", "text echo mismatch"
            ws.send_binary(b"\x00\x01\x02")
            assert ws.recv() == b"\x00\x01\x02", "binary echo mismatch"
        finally:
            ws.close()
    finally:
        app.stop()
        cli.stop()


def test_cli_proxy_websocket_passthrough_404() -> None:
    app_port = free_port()
    info_port = free_port()
    cli = _front([info_port, app_port])
    app = _echo_backend(app_port)
    try:
        assert wait_port(info_port, timeout=25), "front door did not come up" + cli.output()
        _wait_backend(app_port)

        # A non-101 backend answer must pass through untouched (#92
        # semantics) instead of being masked as the landing page.
        try:
            websocket.create_connection(
                f"ws://127.0.0.1:{info_port}/missing",
                cookie="__malkuth_nonce=1",
                timeout=5,
            )
        except websocket.WebSocketBadStatusException as e:
            assert "404" in str(e), f"expected 404 passthrough, got {e!r}"
        else:
            raise AssertionError("handshake to /missing unexpectedly succeeded")
    finally:
        app.stop()
        cli.stop()


def test_cli_proxy_websocket_masked_when_backend_down() -> None:
    info_port = free_port()
    # Point the front door at a port with nothing listening.
    cli = _front([info_port, free_port()])
    try:
        assert wait_port(info_port, timeout=25), "front door did not come up" + cli.output()
        # Backend unreachable → the landing page is served even to upgrade
        # handshakes (200, HTML), i.e. the WS handshake cannot succeed.
        try:
            websocket.create_connection(
                f"ws://127.0.0.1:{info_port}/api/rpc?workspace=test",
                cookie="__malkuth_nonce=1",
                timeout=5,
            )
        except websocket.WebSocketBadStatusException as e:
            assert "200" in str(e), f"expected landing-page 200, got {e!r}"
        else:
            raise AssertionError("handshake to a down backend unexpectedly succeeded")
    finally:
        cli.stop()


if __name__ == "__main__":
    test_cli_proxy_websocket_echo()
    test_cli_proxy_websocket_passthrough_404()
    test_cli_proxy_websocket_masked_when_backend_down()
    print("test_cli_proxy_websocket: PASS")

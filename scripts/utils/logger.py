"""Tiny color logger for malkuth scripts/ tests."""
import os

_NO_COLOR = os.environ.get("NO_COLOR")


def _c(code: str, msg: str) -> str:
    if _NO_COLOR:
        return msg
    return f"\033[{code}m{msg}\033[0m"


def info(msg: str) -> None:
    print(f"  {_c('36', 'ℹ')} {msg}")


def ok(msg: str) -> None:
    print(f"  {_c('32', '✓')} {msg}")


def fail(msg: str) -> None:
    print(f"  {_c('31', '✗')} {msg}")


def section(name: str) -> None:
    print(f"\n{_c('1;35', '▶ ' + name)}")

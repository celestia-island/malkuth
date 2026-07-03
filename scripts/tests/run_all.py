"""Run every malkuth integration test_* module and report a summary."""
import importlib, sys, pathlib, time, traceback

HERE = pathlib.Path(__file__).resolve()
sys.path.insert(0, str(HERE.parent))          # scripts/tests/  (_harness)
sys.path.insert(0, str(HERE.parents[1]))      # scripts/        (utils)
from _harness import ensure_built  # noqa: E402
from utils import logger  # noqa: E402

MODULES = [
    "test_cli_proxy_basic",
    "test_cli_proxy_sticky_crash",
    "test_cli_rolling_restart",
    "test_app_supervise_restart",
    "test_app_rolling_update",
]


def main() -> int:
    ensure_built()
    passed, failed = 0, 0
    for mod_name in MODULES:
        logger.section(mod_name)
        mod = importlib.import_module(mod_name)
        test_fns = [v for k, v in sorted(vars(mod).items()) if k.startswith("test_") and callable(v)]
        for fn in test_fns:
            t0 = time.time()
            try:
                fn()
                logger.ok(f"{fn.__name__}  ({time.time() - t0:.1f}s)")
                passed += 1
            except Exception as e:
                logger.fail(f"{fn.__name__}: {e}")
                traceback.print_exc()
                failed += 1
    print(f"\n==== {passed} passed, {failed} failed ====")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())

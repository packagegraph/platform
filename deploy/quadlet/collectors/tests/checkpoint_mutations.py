"""Fail closed: require a green baseline and an assertion failure per mutation.

Only disposable copies of the wrapper are modified. Run with PG_COLLECT_BIN set
to the just-built binary. An infrastructure error does not count as a kill.
"""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent
SOURCE = (HERE.parent / "scripts/fedora-44-full.sh").read_text()
TEST = "LifecycleTest.test_failed_upload_replays_then_success_retires_and_rederives"
COMMIT = 'pg-collect checkpoint commit --cache-dir "${CACHE_DIR}"'
UPLOAD = '/app/scripts/upload-nt.sh "$RUN_DIR/fedora-44.nt" "$GRAPH_URI"'


def run(wrapper=None):
    # PYTHON_COLORS=0: since 3.14 unittest colorizes its output, splitting the
    # "FAIL: <test>" marker below with ANSI codes. Without this the harness
    # reports every mutation as a survivor even when all of them were killed.
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1", PYTHON_COLORS="0")
    env.pop("CHECKPOINT_TEST_WRAPPER", None)
    if wrapper:
        env["CHECKPOINT_TEST_WRAPPER"] = str(wrapper)
    return subprocess.run([sys.executable, str(HERE / "test_checkpoint_lifecycle.py"), TEST],
                          env=env, capture_output=True, text=True, timeout=100)


def main():
    baseline = run()
    if baseline.returncode:
        raise AssertionError(f"baseline failed:\n{baseline.stdout}\n{baseline.stderr}")
    print("PASS baseline", flush=True)
    # Guard splice cardinality so a renamed command cannot quietly become an
    # unchanged mutant, or mutate a comment instead of the executable command.
    assert SOURCE.count(COMMIT) == 1 and SOURCE.count(UPLOAD) == 1
    mutations = {
        "missing commit": SOURCE.replace(COMMIT, ":"),
        "commit before upload": SOURCE.replace(COMMIT, ":").replace(UPLOAD, COMMIT + "\n" + UPLOAD),
        "swallowed upload failure": SOURCE.replace(UPLOAD, UPLOAD + " || true"),
        "missing exclusions": SOURCE.replace("--exclude 'output/*'", ""),
        "missing errexit": SOURCE.replace("set -eu", "set -u"),
    }
    with tempfile.TemporaryDirectory(prefix="checkpoint-mutations-") as scratch:
        wrapper = Path(scratch) / "mutant.sh"
        for name, source in mutations.items():
            assert source != SOURCE, f"mutation did not apply: {name}"
            wrapper.write_text(source)
            result = run(wrapper)
            if result.returncode != 1 or "FAIL: test_failed_upload" not in result.stderr:
                raise AssertionError(f"mutation survived or errored: {name}\n{result.stdout}\n{result.stderr}")
            print(f"KILLED {name}", flush=True)


if __name__ == "__main__":
    main()

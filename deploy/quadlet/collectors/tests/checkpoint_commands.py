#!/usr/bin/env python3
"""Test-only external-command adapters; never installed with the collectors.

pg-collect still executes the real binary. Only network-source arguments are
redirected; the spec stage (not checkpointed yet) is omitted to prevent live
dist-git requests. mc models remote object storage, not collector/checkpoint or
upload logic. Actual mc wildcard semantics need the separate mc check.
"""
import fnmatch
import json
import os
from pathlib import Path
import shutil
import sys
import time

root = Path(os.environ["CHECKPOINT_TEST_ROOT"])
command = Path(sys.argv[0]).name
args = sys.argv[1:]

if command == "pg-collect":
    if args[0] == "rpm-full":
        if os.environ.get("FAIL_COLLECT"):
            sys.exit(43)
        rewritten = [args[0]]
        iterator = iter(args[1:])
        for arg in iterator:
            if arg in ("--with-spec", "--with-maintainers"):
                continue
            if arg in ("--url", "--koji-hub"):
                next(iterator)
                rewritten += [arg, os.environ["CHECKPOINT_TEST_HUB"] +
                              ("/repo" if arg == "--url" else "/kojihub")]
            else:
                rewritten.append(arg)
        args = rewritten
    os.execv(os.environ["PG_COLLECT_BIN"], [os.environ["PG_COLLECT_BIN"], *args])

elif command == "sleep":
    # Exercise the periodic mirror once per invocation; then block WITHOUT
    # inheriting captured output pipes, so the wrapper's EXIT trap is enough.
    marker = root / "periodic-started"
    if not marker.exists():
        marker.touch()
        sys.exit(0)
    os.close(1)
    os.close(2)
    time.sleep(2)

elif command == "mc":
    with (root / "mc-calls").open("a") as log:
        log.write(json.dumps(args) + "\n")

    def local(path):
        if path.startswith("pgraph/test/"):
            return root / "remote" / path.removeprefix("pgraph/test/")
        target = Path(path)
        assert target.is_relative_to(root), f"test escaped sandbox: {path}"
        return target

    if args[:2] == ["alias", "set"]:
        sys.exit(0)
    if args[0] == "mirror":
        source, target = map(local, args[-2:])
        excluded = args[args.index("--exclude") + 1] if "--exclude" in args else None
        for path in source.rglob("*"):
            relative = path.relative_to(source)
            if path.is_file() and not (excluded and fnmatch.fnmatch(str(relative), excluded)):
                destination = target / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(path, destination)
    elif args[0] in ("cp", "pipe"):
        if os.environ.get("FAIL_UPLOAD") == args[0]:
            sys.exit(42)
        destination = local(args[-1])
        destination.parent.mkdir(parents=True, exist_ok=True)
        if args[0] == "cp":
            shutil.copyfile(local(args[1]), destination)
        else:
            destination.write_bytes(sys.stdin.buffer.read())
    else:
        raise AssertionError(f"unexpected mc arguments: {args}")
else:
    raise AssertionError(f"unexpected adapter: {command}")

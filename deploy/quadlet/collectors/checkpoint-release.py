#!/usr/bin/env python3
"""Stage/verify a pinned collector release. Does not stop services or install it.

Run from the checkout used to build IMAGE. See checkpoint-cutover.md for the
maintenance boundary and the required effective-systemd checks. Verifying files
is deliberately not described as verifying the live host or the image's origin.
"""
import argparse
from pathlib import Path
import re
import subprocess
import sys

SOURCE = Path(__file__).resolve().parent
TEMPLATES = ("pg-collect@.container", "pg-collect-rhel@.container")


def release_files(image):
    if not re.fullmatch(r"ghcr\.io/packagegraph/etl@sha256:[0-9a-f]{64}", image):
        raise ValueError("IMAGE must be an immutable ghcr.io/packagegraph/etl@sha256:<64 hex> digest")
    files = {}
    for name in TEMPLATES:
        lines = (SOURCE / name).read_text().splitlines()
        if sum(line.startswith("Image=") for line in lines) != 1:
            raise ValueError(f"expected exactly one Image= in {name}")
        files[name] = ("\n".join(
            f"Image={image}" if line.startswith("Image=") else line
            for line in lines if not line.startswith("AutoUpdate=")
        ) + "\n").encode()
    wrappers = [p for p in (SOURCE / "scripts").glob("*-full.sh")
                if "pg-collect rpm-full" in p.read_text()]
    if not wrappers:
        raise ValueError("no rpm-full wrappers found; refusing an empty release")
    for path in wrappers + [SOURCE / "scripts/test-wrapper-checkpoint-contract.sh"]:
        files[f"scripts/collectors/{path.name}"] = path.read_bytes()
    return files


def verify(root, files):
    # Local overrides can negate the Image= pin. Do not silently remove them;
    # require operator reconciliation. Other Quadlet search paths and systemd
    # service overrides are checked separately by the cutover runbook.
    overrides = list(root.glob("container.d/*.conf")) + list(root.glob("pg-*.container.d/*.conf"))
    overrides += [p for p in root.glob("pg-collect*@*.container") if p.name not in TEMPLATES]
    if overrides:
        raise ValueError(f"reconcile collector overrides before resuming: {overrides}")
    for name, expected in files.items():
        path = root / name
        if path.is_symlink() or path.read_bytes() != expected:
            raise ValueError(f"installed file differs from this release: {path}")
        if name.endswith(".sh") and not path.stat().st_mode & 0o111:
            raise ValueError(f"installed script is not executable: {path}")
    subprocess.run(["bash", str(root / "scripts/collectors/test-wrapper-checkpoint-contract.sh")],
                   check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("stage", "verify"))
    parser.add_argument("image")
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    try:
        files = release_files(args.image)
        if args.action == "stage":
            # Never overwrite an installation or an earlier rollback bundle.
            args.directory.mkdir(parents=False, exist_ok=False)
            for name, body in files.items():
                path = args.directory / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(body)
                path.chmod(0o755 if name.endswith(".sh") else 0o644)
        verify(args.directory, files)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print(f"Verified release files for {args.image}; effective host units still require verification.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

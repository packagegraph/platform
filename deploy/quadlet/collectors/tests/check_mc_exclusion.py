"""Characterize the pinned mc's real wildcard behavior with local directories.

This complements the lifecycle test's object-store adapter. No S3 credentials
or network requests; assertions require source siblings to actually copy.
"""
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="checkpoint-mc-") as scratch:
        root = Path(scratch)
        source, remote, warm = (root / name for name in ("cache", "remote", "warm"))
        for directory in (source, remote, warm):
            directory.mkdir()
        for name in ("output/GENERATION", "output/gen/koji/v1/item", "koji/source.json"):
            file = source / name
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text(name)

        def mirror(origin, destination):
            subprocess.run([binary, "--config-dir", str(root / "config"), "mirror", "--overwrite",
                            "--exclude", "output/*", str(origin) + "/", str(destination) + "/"],
                           check=True, timeout=30)

        mirror(source, remote)
        assert (remote / "koji/source.json").read_text() == "koji/source.json"
        assert not list(remote.glob("output/**/*")), "checkpoint escaped push exclusion"
        # Simulate a legacy remote checkpoint. Warming must neither import it
        # nor overwrite/delete a local active generation.
        (remote / "output").mkdir(exist_ok=True)
        (remote / "output/GENERATION").write_text("remote stale")
        (warm / "output").mkdir()
        (warm / "output/GENERATION").write_text("local active")
        mirror(remote, warm)
        assert (warm / "output/GENERATION").read_text() == "local active"
        assert (warm / "koji/source.json").read_text() == "koji/source.json"
    print("PASS: real mc excludes checkpoints in both directions and copies source siblings")


if __name__ == "__main__":
    main()

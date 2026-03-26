"""TDB2 index builder using Apache Jena."""

import subprocess
import tarfile
from pathlib import Path


class TDB2Builder:
    """Builds TDB2 indexes from RDF files using Apache Jena's tdb2.tdbloader."""

    def __init__(self, jena_home: str = "/opt/jena"):
        self.jena_home = Path(jena_home)

    def build(self, input_files: list[Path], output_dir: Path) -> None:
        """Run tdb2.tdbloader to build a TDB2 index.

        Raises RuntimeError on non-zero exit code.
        """
        tdbloader = str(self.jena_home / "bin" / "tdb2.tdbloader")
        cmd = [tdbloader, f"--loc={output_dir}", *[str(f) for f in input_files]]

        result = subprocess.run(cmd, capture_output=True, text=True)
        if result.returncode != 0:
            raise RuntimeError(
                f"tdb2.tdbloader failed (exit {result.returncode}): {result.stderr}"
            )

    def package(self, tdb_dir: Path, output_path: Path) -> None:
        """Create a tar.gz archive of the TDB2 directory."""
        tdb_dir = Path(tdb_dir)
        output_path = Path(output_path)
        with tarfile.open(output_path, "w:gz") as tar:
            tar.add(tdb_dir, arcname=tdb_dir.name)

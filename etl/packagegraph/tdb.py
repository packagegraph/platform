"""TDB2 index builder using Apache Jena."""

import subprocess
import tarfile
from pathlib import Path


class TDB2Builder:
    """Builds TDB2 indexes from RDF files using Apache Jena's tdb2.tdbloader."""

    def __init__(self, jena_home: str = "/opt/jena"):
        self.jena_home = Path(jena_home)

    def build(
        self,
        input_files: list[Path],
        output_dir: Path,
        ontology_files: list[Path] | None = None,
        ontology_graph: str = "https://packagegraph.github.io/ontology",
    ) -> None:
        """Run tdb2.tdbloader to build a TDB2 index.

        Data files are loaded into the default graph.
        Ontology files are loaded into a separate named graph to keep
        the TBox (class/property definitions) out of the ABox (instance data).

        Raises RuntimeError on non-zero exit code.
        """
        tdbloader = str(self.jena_home / "bin" / "tdb2.tdbloader")

        # Load data files into default graph
        if input_files:
            cmd = [tdbloader, f"--loc={output_dir}", *[str(f) for f in input_files]]
            result = subprocess.run(cmd, capture_output=True, text=True)
            if result.returncode != 0:
                raise RuntimeError(
                    f"tdb2.tdbloader failed on data files (exit {result.returncode}): {result.stderr}"
                )

        # Load ontology files into named graph
        if ontology_files:
            cmd = [
                tdbloader,
                f"--loc={output_dir}",
                f"--graph={ontology_graph}",
                *[str(f) for f in ontology_files],
            ]
            result = subprocess.run(cmd, capture_output=True, text=True)
            if result.returncode != 0:
                raise RuntimeError(
                    f"tdb2.tdbloader failed on ontology files (exit {result.returncode}): {result.stderr}"
                )

    def package(self, tdb_dir: Path, output_path: Path) -> None:
        """Create a tar.gz archive of the TDB2 directory."""
        tdb_dir = Path(tdb_dir)
        output_path = Path(output_path)
        with tarfile.open(output_path, "w:gz") as tar:
            tar.add(tdb_dir, arcname=tdb_dir.name)

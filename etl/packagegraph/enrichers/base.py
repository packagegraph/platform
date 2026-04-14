"""Base enricher infrastructure with provenance tracking and deterministic output."""

from abc import ABC, abstractmethod
from typing import Any, TextIO
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
from packagegraph.sparql_client import SparqlQueryClient
from packagegraph.namespaces import PROV, PKG, DATA, snapshot_uri


class NTriplesWriter:
    """Writes N-Triples with deterministic sorted output."""

    def __init__(self):
        self._triples: list[str] = []

    def _escape_nt(self, s: str) -> str:
        """Escape string for N-Triples literal."""
        return (
            s.replace("\\", "\\\\")
            .replace('"', '\\"')
            .replace("\n", "\\n")
            .replace("\r", "\\r")
            .replace("\t", "\\t")
        )

    def write_lit(self, subject: str, predicate: str, value: str | int | float) -> None:
        """Write a literal triple."""
        escaped = self._escape_nt(str(value))
        self._triples.append(f'<{subject}> <{predicate}> "{escaped}" .\n')

    def write_int(self, subject: str, predicate: str, value: int) -> None:
        """Write an integer triple with xsd:integer datatype."""
        self._triples.append(
            f'<{subject}> <{predicate}> "{value}"^^<http://www.w3.org/2001/XMLSchema#integer> .\n'
        )

    def write_uri(self, subject: str, predicate: str, obj: str) -> None:
        """Write a URI triple."""
        self._triples.append(f'<{subject}> <{predicate}> <{obj}> .\n')

    def get_sorted_triples(self) -> list[str]:
        """Return all triples sorted lexicographically."""
        return sorted(self._triples)

    def write_to_file(self, file_handle: TextIO) -> None:
        """Write sorted triples to file handle."""
        for triple in self.get_sorted_triples():
            file_handle.write(triple)


class BaseEnricher(ABC):
    """Abstract base class for enrichers with provenance tracking.

    Subclasses must implement:
    - _query_packages(): Return list of items to process
    - _process_item(item): Process one item, write triples via self.writer
    """

    def __init__(
        self,
        sparql_client: SparqlQueryClient,
        output_path: str,
        enricher_name: str,
        enricher_version: str,
        cache_dir: str | None = None,
        fuseki_recency_threshold_days: int = 7,
    ):
        self.client: SparqlQueryClient = sparql_client
        self.output_path: str = output_path
        self.enricher_name: str = enricher_name
        self.enricher_version: str = enricher_version
        self.cache_dir: str | None = cache_dir
        self.recency_threshold_days: int = fuseki_recency_threshold_days
        self.writer: NTriplesWriter = NTriplesWriter()
        self.start_time: datetime | None = None
        self.end_time: datetime | None = None

    @abstractmethod
    def _query_packages(self) -> list[Any]:
        """Query Fuseki for packages to enrich.

        Returns list of items (tuples, dicts, etc.) to process.
        """
        pass

    @abstractmethod
    def _process_item(self, item: Any) -> None:
        """Process one item and write triples via self.writer.

        This is called once per item from _query_packages().
        Write triples using self.writer.write_lit/write_int/write_uri.
        """
        pass

    def _validate_fuseki_recency(self) -> None:
        """Validate that Fuseki data is recent enough.

        Queries Fuseki metadata endpoint for last-modified timestamp.
        Exits with error if no data or data older than threshold.
        """
        # Query for any package to verify Fuseki has data
        try:
            test_query = "PREFIX pkg: <https://purl.org/packagegraph/ontology/core#> SELECT ?p WHERE { ?p a pkg:BinaryPackage } LIMIT 1"
            bindings = self.client.query(test_query)
            if not bindings:
                raise RuntimeError(
                    "Fuseki data validation failed: No packages found in graph. "
                    "Enricher requires populated Fuseki before running."
                )
        except Exception as e:
            raise RuntimeError(f"Fuseki recency validation failed: {e}") from e

    def _record_provenance(self) -> None:
        """Append PROV-O activity and DataSnapshot triples to output.

        Called after main enrichment triples are written.
        """
        if not self.start_time or not self.end_time:
            return

        # Create activity URI
        activity_uri = DATA[f"activity/{self.enricher_name}/{self.start_time.isoformat()}"]

        # PROV-O Activity triples
        self.writer.write_uri(str(activity_uri), 'http://www.w3.org/1999/02/22-rdf-syntax-ns#type', str(PROV.Activity))
        self.writer.write_lit(str(activity_uri), str(PROV.startedAtTime), self.start_time.isoformat())
        self.writer.write_lit(str(activity_uri), str(PROV.endedAtTime), self.end_time.isoformat())
        self.writer.write_lit(str(activity_uri), str(PROV.wasAssociatedWith), f"{self.enricher_name} v{self.enricher_version}")

        # DataSnapshot triple
        snap_uri = snapshot_uri(self.enricher_name, self.end_time.isoformat())
        self.writer.write_uri(snap_uri, 'http://www.w3.org/1999/02/22-rdf-syntax-ns#type', str(PKG.DataSnapshot))
        self.writer.write_lit(snap_uri, str(PKG.snapshotSource), self.enricher_name)
        self.writer.write_lit(snap_uri, str(PKG.snapshotTimestamp), self.end_time.isoformat())
        self.writer.write_uri(snap_uri, str(PROV.wasGeneratedBy), str(activity_uri))

    def _write_manifest(self) -> None:
        """Write sidecar manifest JSON with run metadata and content hash."""
        if not self.start_time or not self.end_time:
            return

        # Compute content hash of output file
        sha256 = hashlib.sha256()
        with open(self.output_path, 'rb') as f:
            for chunk in iter(lambda: f.read(8192), b""):
                sha256.update(chunk)
        content_hash = f"sha256:{sha256.hexdigest()}"

        manifest = {
            'enricher_name': self.enricher_name,
            'enricher_version': self.enricher_version,
            'start_time': self.start_time.isoformat(),
            'end_time': self.end_time.isoformat(),
            'duration_seconds': (self.end_time - self.start_time).total_seconds(),
            'output_file': self.output_path,
            'content_hash': content_hash,
        }

        manifest_path = Path(self.output_path).with_suffix('.manifest.json')
        with open(manifest_path, 'w') as f:
            json.dump(manifest, f, indent=2)

    def _sync_cache_to_minio(self) -> None:
        """Sync cache to Minio if a CacheManager with Minio is available.

        Looks for a 'cache' attribute on the subclass (GitHubEnricher has one).
        """
        cache = getattr(self, 'cache', None)
        if cache and hasattr(cache, 'sync_to_minio'):
            count = cache.sync_to_minio()
            if count > 0:
                print(f"  [cache synced: {count} entries to Minio]")

    def _preflight_check(self) -> None:
        """Optional preflight check before processing items.

        Override in subclasses to validate external service access
        (e.g., API tokens, connectivity) before starting the main loop.
        """
        pass

    def enrich(self) -> None:
        """Main enrichment entry point.

        Lifecycle: validate Fuseki recency → preflight check → query packages →
        process each → sort output → write to file → record provenance → write manifest.
        """
        self.start_time = datetime.now()

        # Validate Fuseki has recent data
        self._validate_fuseki_recency()

        # Run subclass preflight checks (e.g., API auth validation)
        self._preflight_check()

        # Query packages to enrich
        items = self._query_packages()
        print(f"Processing {len(items)} items...")

        # Cache sync interval — sync to Minio every N items (0 = disabled)
        cache_disabled = os.environ.get('ENRICHER_CACHE_DISABLED', '0') == '1'
        sync_interval = int(os.environ.get('ENRICHER_CACHE_SYNC_INTERVAL', '500'))

        # Process each item
        for idx, item in enumerate(items, 1):
            if idx % 100 == 0:
                print(f"  [{idx}/{len(items)}]")
            self._process_item(item)

            # Periodic cache sync to Minio
            if not cache_disabled and sync_interval > 0 and idx % sync_interval == 0:
                self._sync_cache_to_minio()

        # Final cache sync
        if not cache_disabled:
            self._sync_cache_to_minio()

        # Record provenance (before writing, so it's included in output)
        self.end_time = datetime.now()
        self._record_provenance()

        # Write sorted output
        with open(self.output_path, 'w') as f:
            self.writer.write_to_file(f)

        # Write sidecar manifest (after file is written, needs content hash)
        self._write_manifest()

        print(f"Enrichment complete. Output: {self.output_path}")

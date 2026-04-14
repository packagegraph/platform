"""Tests for provenance recording and DataSnapshot creation."""

import pytest
import json
from unittest.mock import MagicMock, patch
from packagegraph.enrichers.base import BaseEnricher


class _ProvenanceTestEnricher(BaseEnricher):
    """Concrete enricher for testing provenance."""

    def _query_packages(self):
        return [('pkg1', 'http://example.org/pkg1')]

    def _process_item(self, item):
        name, uri = item
        self.writer.write_lit(uri, 'http://example.org/name', name)


@pytest.mark.unit
class TestProvenanceRecording:
    def test_enrich_produces_prov_activity_triples(self, tmp_path):
        """Test that enrichment run produces prov:Activity triples."""
        output_file = tmp_path / 'output.nt'
        mock_client = MagicMock()

        enricher = _ProvenanceTestEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            enricher_name='test_enricher',
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        # Read output and check for provenance triples
        content = output_file.read_text()
        assert 'prov#Activity' in content
        assert 'prov#startedAtTime' in content
        assert 'prov#endedAtTime' in content
        assert 'prov#wasAssociatedWith' in content
        assert 'test_enricher' in content
        assert '1.0.0' in content

    def test_enrich_produces_data_snapshot_triple(self, tmp_path):
        """Test that enrichment run produces pkg:DataSnapshot triple."""
        output_file = tmp_path / 'output.nt'
        mock_client = MagicMock()

        enricher = _ProvenanceTestEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            enricher_name='snapshot_test',
            enricher_version='2.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        assert 'DataSnapshot' in content
        assert 'snapshotSource' in content
        assert 'snapshot_test' in content
        assert 'snapshotTimestamp' in content

    def test_sidecar_manifest_created(self, tmp_path):
        """Test that sidecar manifest JSON is written alongside N-Triples."""
        output_file = tmp_path / 'output.nt'
        manifest_file = tmp_path / 'output.manifest.json'
        mock_client = MagicMock()

        enricher = _ProvenanceTestEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            enricher_name='manifest_test',
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        # Check manifest exists
        assert manifest_file.exists()

        # Validate manifest structure
        with open(manifest_file) as f:
            manifest = json.load(f)

        assert manifest['enricher_name'] == 'manifest_test'
        assert manifest['enricher_version'] == '1.0.0'
        assert 'start_time' in manifest
        assert 'end_time' in manifest
        assert 'duration_seconds' in manifest
        assert 'output_file' in manifest
        assert 'content_hash' in manifest
        assert manifest['content_hash'].startswith('sha256:')

    def test_content_hash_in_manifest_matches_output(self, tmp_path):
        """Test that content hash in manifest matches actual file hash."""
        import hashlib

        output_file = tmp_path / 'output.nt'
        manifest_file = tmp_path / 'output.manifest.json'
        mock_client = MagicMock()

        enricher = _ProvenanceTestEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            enricher_name='hash_test',
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        # Compute actual hash
        sha256 = hashlib.sha256()
        with open(output_file, 'rb') as f:
            for chunk in iter(lambda: f.read(8192), b""):
                sha256.update(chunk)
        expected_hash = f"sha256:{sha256.hexdigest()}"

        # Check manifest hash
        with open(manifest_file) as f:
            manifest = json.load(f)

        assert manifest['content_hash'] == expected_hash

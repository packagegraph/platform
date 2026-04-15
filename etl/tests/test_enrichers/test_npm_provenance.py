"""Tests for npm provenance enricher."""

import pytest
from unittest.mock import MagicMock, patch
from packagegraph.enrichers.npm_provenance import NpmProvenanceEnricher


@pytest.mark.unit
class TestNpmProvenanceEnricher:
    def test_enrich_produces_provenance_attestation_triples(self, tmp_path):
        """Test that npm provenance enricher produces SLSA attestation triples."""
        output_file = tmp_path / 'npm_provenance.nt'
        mock_client = MagicMock()
        mock_client.query_packages_by_type.return_value = [
            ("@npmcli/config", "8.0.0")
        ]

        npm_resp = MagicMock()
        npm_resp.status_code = 200
        npm_resp.json.return_value = {
            "name": "@npmcli/config",
            "version": "8.0.0",
            "dist": {
                "attestations": {
                    "predicateType": "https://slsa.dev/provenance/v1",
                    "bundle": {}
                }
            }
        }
        npm_resp.raise_for_status.return_value = None

        with patch("packagegraph.enrichers.npm_provenance.requests.get", return_value=npm_resp):
            enricher = NpmProvenanceEnricher(
                sparql_client=mock_client,
                output_path=str(output_file)
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output_file.read_text()
        assert "ProvenanceAttestation" in content
        assert "hasProvenance" in content
        assert "attestsBuildLevel" in content
        assert "slsa#L2" in content or "L2" in content

    def test_enrich_handles_missing_attestations(self, tmp_path):
        """Test that packages without attestations are skipped gracefully."""
        output_file = tmp_path / 'npm_provenance.nt'
        mock_client = MagicMock()
        mock_client.query_packages_by_type.return_value = [
            ("lodash", "4.17.21")
        ]

        npm_resp = MagicMock()
        npm_resp.status_code = 200
        npm_resp.json.return_value = {
            "name": "lodash",
            "version": "4.17.21",
            "dist": {}  # No attestations
        }
        npm_resp.raise_for_status.return_value = None

        with patch("packagegraph.enrichers.npm_provenance.requests.get", return_value=npm_resp):
            enricher = NpmProvenanceEnricher(
                sparql_client=mock_client,
                output_path=str(output_file)
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output_file.read_text()
        # Should only have provenance metadata, no package-specific attestations
        assert "hasProvenance" not in content or content.count("hasProvenance") == 0

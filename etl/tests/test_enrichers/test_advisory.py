"""Tests for advisory enrichers (RHSA, DSA)."""

import pytest
from unittest.mock import MagicMock, patch
from packagegraph.enrichers.advisory import RHSAEnricher, DSAEnricher


@pytest.mark.unit
class TestRHSAEnricher:
    def test_enrich_produces_rhsa_advisory_triples(self, tmp_path):
        """Test that RHSA enricher produces SecurityAdvisory triples."""
        output_file = tmp_path / 'advisory_rhsa.nt'
        mock_client = MagicMock()

        rhsa_resp = [
            {
                "CVE": "CVE-2023-1234",
                "severity": "Important",
                "public_date": "2023-05-01T00:00:00Z",
                "bugzilla_description": "RHSA-2023:1234",
                "affected_packages": []
            }
        ]

        with patch("packagegraph.enrichers.advisory.requests.get") as mock_get:
            mock_resp = MagicMock()
            mock_resp.status_code = 200
            mock_resp.json.return_value = rhsa_resp
            mock_resp.raise_for_status.return_value = None
            mock_get.return_value = mock_resp

            enricher = RHSAEnricher(
                sparql_client=mock_client,
                output_path=str(output_file),
                cache_dir=str(tmp_path / "cache"),
                days_back=30
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output_file.read_text()
        assert "SecurityAdvisory" in content
        assert "RHSA-2023:1234" in content
        assert "addressesVulnerability" in content
        assert "CVE-2023-1234" in content


@pytest.mark.unit
class TestDSAEnricher:
    def test_enrich_produces_dsa_advisory_triples(self, tmp_path):
        """Test that DSA enricher produces SecurityAdvisory triples."""
        output_file = tmp_path / 'advisory_dsa.nt'
        mock_client = MagicMock()

        dsa_resp = {
            "curl": {
                "CVE-2023-5678": {
                    "debianbug": "DSA-5432-1",
                    "description": "Buffer overflow"
                }
            }
        }

        with patch("packagegraph.enrichers.advisory.requests.get") as mock_get:
            mock_resp = MagicMock()
            mock_resp.status_code = 200
            mock_resp.json.return_value = dsa_resp
            mock_resp.raise_for_status.return_value = None
            mock_get.return_value = mock_resp

            enricher = DSAEnricher(
                sparql_client=mock_client,
                output_path=str(output_file),
                cache_dir=str(tmp_path / "cache")
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        content = output_file.read_text()
        assert "SecurityAdvisory" in content
        assert "DSA-5432-1" in content
        assert "addressesVulnerability" in content
        assert "CVE_2023_5678" in content

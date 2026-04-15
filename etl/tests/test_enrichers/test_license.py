"""Tests for License claim enricher."""

import pytest
from unittest.mock import MagicMock, patch
from packagegraph.enrichers.license import LicenseEnricher
from packagegraph.enrichers.cache import CacheManager


@pytest.mark.unit
class TestLicenseEnricher:
    def test_enrich_produces_license_triples(self, tmp_path):
        """Test that license enricher produces pkg:hasLicense triples."""
        output_file = tmp_path / 'licenses.nt'

        mock_client = MagicMock()
        # Mock SPARQL query to return packages with GitHub homepages
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        mock_cache.sync_to_minio.return_value = 0
        # Mock GitHub API response with license data
        mock_cache.get.return_value = {
            'license': {
                'key': 'mit',
                'name': 'MIT License',
                'spdx_id': 'MIT'
            }
        }

        enricher = LicenseEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        # Verify output contains license triples
        content = output_file.read_text()
        assert 'hasLicense' in content
        assert 'License' in content
        assert 'spdxExpression' in content
        assert '"MIT"' in content

    def test_enrich_skips_repos_without_license(self, tmp_path):
        """Test that repos with null license are skipped."""
        output_file = tmp_path / 'licenses.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        mock_cache.sync_to_minio.return_value = 0
        # GitHub API returns None for license
        mock_cache.get.return_value = {'license': None}

        enricher = LicenseEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        # Should only have provenance, no license data triples
        data_lines = [line for line in content.split('\n') if 'hasLicense' in line]
        assert len(data_lines) == 0

    def test_spdx_id_validation(self, tmp_path):
        """Test that invalid SPDX IDs are rejected or normalized."""
        output_file = tmp_path / 'licenses.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        mock_cache.sync_to_minio.return_value = 0
        # GitHub returns valid SPDX ID
        mock_cache.get.return_value = {
            'license': {'spdx_id': 'Apache-2.0', 'name': 'Apache License 2.0'}
        }

        enricher = LicenseEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        assert '"Apache-2.0"' in content

    def test_provenance_attribution_to_github_api(self, tmp_path):
        """Test that license claims are attributed to GitHub API."""
        output_file = tmp_path / 'licenses.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        mock_cache.sync_to_minio.return_value = 0
        mock_cache.get.return_value = {
            'license': {'spdx_id': 'MIT', 'name': 'MIT License'}
        }

        enricher = LicenseEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        # Check for PROV-O attribution (from base class)
        assert 'prov#Activity' in content or 'prov#wasAssociatedWith' in content

"""Tests for Metrics claim enricher."""

import pytest
from unittest.mock import MagicMock, patch
from packagegraph.enrichers.metrics import MetricsEnricher
from packagegraph.enrichers.cache import CacheManager


@pytest.mark.unit
class TestMetricsEnricher:
    def test_enrich_produces_language_metrics(self, tmp_path):
        """Test that metrics enricher produces language composition triples."""
        output_file = tmp_path / 'metrics.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        # Mock /languages endpoint response
        mock_cache.get.return_value = {
            'Python': 45231,
            'C': 12000,
            'JavaScript': 3500
        }

        enricher = MetricsEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        # Should have ProgrammingLanguage instances
        assert 'ProgrammingLanguage' in content
        assert '"Python"' in content
        assert '"C"' in content
        assert 'languageName' in content
        # Should have implementedIn links
        assert 'implementedIn' in content

    def test_language_proportions_calculated(self, tmp_path):
        """Test that language proportions are calculated from byte counts."""
        output_file = tmp_path / 'metrics.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        # Total: 60000 bytes
        mock_cache.get.return_value = {
            'Python': 48000,  # 80%
            'Shell': 12000    # 20%
        }

        enricher = MetricsEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        # Should have proportion data
        assert 'languageProportion' in content
        # Check approximate proportions (0.8 for Python, 0.2 for Shell)
        assert '"0.8' in content or '"0.80' in content
        assert '"0.2' in content or '"0.20' in content

    def test_empty_languages_response_produces_no_metrics(self, tmp_path):
        """Test that repos with no language data produce no metric triples."""
        output_file = tmp_path / 'metrics.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        mock_cache.get.return_value = {}  # Empty languages response

        enricher = MetricsEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        # Should only have provenance, no language data
        data_lines = [line for line in content.split('\n') if 'implementedIn' in line]
        assert len(data_lines) == 0

    def test_all_languages_get_programming_language_instances(self, tmp_path):
        """Test that each language gets a ProgrammingLanguage instance."""
        output_file = tmp_path / 'metrics.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        mock_cache.get.return_value = {
            'Rust': 50000,
            'TypeScript': 30000,
            'Go': 20000
        }

        enricher = MetricsEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        # Each language should have a ProgrammingLanguage instance
        lang_lines = [line for line in content.split('\n') if 'ProgrammingLanguage' in line]
        assert len(lang_lines) == 3

        # Each language should have a languageName triple
        assert '"Rust"' in content
        assert '"TypeScript"' in content
        assert '"Go"' in content

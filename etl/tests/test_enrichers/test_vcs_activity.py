"""Tests for VCS activity claim enricher."""

import pytest
from unittest.mock import MagicMock, patch
from packagegraph.enrichers.vcs_activity import VCSActivityEnricher
from packagegraph.enrichers.cache import CacheManager


@pytest.mark.unit
class TestVCSActivityEnricher:
    def test_enrich_produces_release_triples(self, tmp_path):
        """Test that VCS activity enricher produces vcs:Release triples."""
        output_file = tmp_path / 'vcs_activity.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        # Mock /releases endpoint response
        mock_cache.get.return_value = [
            {
                'tag_name': 'v1.0.0',
                'name': 'Version 1.0.0',
                'published_at': '2026-01-15T10:00:00Z',
                'prerelease': False
            },
            {
                'tag_name': 'v0.9.0-beta',
                'name': 'Beta Release',
                'published_at': '2025-12-01T08:00:00Z',
                'prerelease': True
            }
        ]

        enricher = VCSActivityEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        # Should have Release instances
        assert 'vcs#Release' in content or 'Release' in content
        assert '"v1.0.0"' in content
        assert 'tagName' in content
        assert 'releaseDate' in content

    def test_prerelease_flag_recorded(self, tmp_path):
        """Test that prerelease status is recorded."""
        output_file = tmp_path / 'vcs_activity.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        mock_cache.get.return_value = [
            {'tag_name': 'v1.0.0-rc1', 'published_at': '2026-01-01T00:00:00Z', 'prerelease': True}
        ]

        enricher = VCSActivityEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        assert 'isPreRelease' in content
        assert '"true"' in content or 'true' in content

    def test_activity_metrics_from_repo_data(self, tmp_path):
        """Test that activity metrics are extracted from repo metadata."""
        output_file = tmp_path / 'vcs_activity.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)

        def cache_get(url):
            if '/releases' in url:
                return []
            # Mock repo metadata
            return {
                'created_at': '2020-01-01T00:00:00Z',
                'pushed_at': '2026-04-10T12:00:00Z',
                'subscribers_count': 42,
                'open_issues_count': 15
            }

        mock_cache.get.side_effect = cache_get

        enricher = VCSActivityEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        # Should have activity metrics
        assert 'firstCommitDate' in content or 'created_at' in content
        assert '2020-01-01' in content

    def test_empty_releases_produces_no_release_triples(self, tmp_path):
        """Test that repos with no releases produce no release triples."""
        output_file = tmp_path / 'vcs_activity.nt'

        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            ('http://example.org/pkg1', 'https://github.com/owner/repo1')
        ]

        mock_cache = MagicMock(spec=CacheManager)
        mock_cache.get.return_value = []  # Empty releases array

        enricher = VCSActivityEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            cache_manager=mock_cache,
            enricher_version='1.0.0'
        )

        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        content = output_file.read_text()
        release_lines = [line for line in content.split('\n') if 'vcs#Release' in line or 'tagName' in line]
        assert len(release_lines) == 0

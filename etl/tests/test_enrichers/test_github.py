from unittest.mock import patch, MagicMock
import pytest
from packagegraph.enrichers.github import GitHubEnricher


def _mock_resp(json_data, status_code=200, headers=None):
    m = MagicMock()
    m.status_code = status_code
    m.json.return_value = json_data
    m.headers = headers or {"X-RateLimit-Remaining": "4999"}
    m.raise_for_status.return_value = None
    return m


@pytest.mark.unit
class TestGitHubEnricher:
    def test_enrich_writes_repo_metadata(self, tmp_path):
        mock_client = MagicMock()
        mock_client.query_github_homepages.return_value = [
            (
                "https://packagegraph.github.io/data/package/debian/trixie/amd64/curl/7.88",
                "https://github.com/curl/curl",
            ),
        ]
        repo = {
            "html_url": "https://github.com/curl/curl",
            "default_branch": "master",
            "description": "A command line tool",
            "stargazers_count": 35000,
            "forks_count": 6000,
        }
        commits = [
            {
                "sha": "abc123def456",
                "commit": {
                    "author": {
                        "name": "Daniel Stenberg",
                        "email": "daniel@haxx.se",
                        "date": "2026-04-01T12:00:00Z",
                    },
                    "message": "Fix buffer overflow",
                },
            }
        ]

        output = tmp_path / "github.nt"
        with patch("packagegraph.enrichers.github.requests.get") as mock_get:
            # Mock both calls - repo data returned for any /repos/ call, commits for /commits call
            def mock_api_call(*args, **kwargs):
                url = args[0] if args else kwargs.get('url', '')
                if '/commits' in url:
                    return _mock_resp(commits)
                else:
                    return _mock_resp(repo)
            mock_get.side_effect = mock_api_call
            enricher = GitHubEnricher(
                mock_client,
                str(output),
                github_token="fake",
                cache_dir=str(tmp_path / "cache"),
            )
            enricher.enrich()

        content = output.read_text()
        assert "Repository" in content
        assert "curl" in content
        assert "35000" in content
        assert "abc123def456" in content

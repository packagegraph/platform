import pytest
from rdflib import Graph, Literal
from rdflib.namespace import RDF
from unittest.mock import Mock, patch
from packagegraph.namespaces import PKG, VCS, DATA
from packagegraph.collectors.github import GitHubEnricher


@pytest.mark.unit
@patch("packagegraph.collectors.github.requests.get")
@patch("packagegraph.collectors.github.time.sleep")
def test_github_enrichment(mock_sleep, mock_get):
    """GitHubEnricher should create vcs:Repository and link to SourcePackage."""
    g = Graph()

    # Add a binary package with GitHub homepage
    pkg_uri = DATA["package/debian/bookworm/amd64/curl/8.4.0-2"]
    g.add((pkg_uri, RDF.type, PKG.BinaryPackage))
    g.add((pkg_uri, PKG.packageName, Literal("curl")))
    g.add((pkg_uri, PKG.homepage, Literal("https://github.com/curl/curl")))

    # Add source package linked from binary
    src_uri = DATA["source/debian/bookworm/curl/8.4.0-2"]
    g.add((src_uri, RDF.type, PKG.SourcePackage))
    g.add((src_uri, PKG.packageName, Literal("curl")))
    g.add((pkg_uri, PKG.builtFromSource, src_uri))

    # Mock GitHub API responses
    repo_response = Mock()
    repo_response.json.return_value = {
        "full_name": "curl/curl",
        "default_branch": "master",
        "description": "A command line tool for transferring data",
        "stargazers_count": 35000,
        "forks_count": 6000,
        "html_url": "https://github.com/curl/curl",
    }
    repo_response.raise_for_status = Mock()
    repo_response.headers = {"X-RateLimit-Remaining": "4999"}

    commits_response = Mock()
    commits_response.json.return_value = [
        {
            "sha": "abc123def456789012345678901234567890abcd",
            "commit": {
                "author": {
                    "name": "Daniel Stenberg",
                    "email": "daniel@haxx.se",
                    "date": "2024-01-15T10:30:00Z",
                },
                "message": "curl: fix URL parsing",
            },
        }
    ]
    commits_response.raise_for_status = Mock()
    commits_response.headers = {"X-RateLimit-Remaining": "4998"}

    mock_get.side_effect = [repo_response, commits_response]

    enricher = GitHubEnricher(g, github_token="fake-token", cache_dir=None)
    with patch("packagegraph.collectors.github.click.echo"):
        enricher.enrich()

    # Verify repository was created
    repo_triples = list(g.triples((None, RDF.type, VCS.Repository)))
    assert len(repo_triples) == 1
    repo_uri = repo_triples[0][0]

    assert (repo_uri, VCS.defaultBranch, Literal("master")) in g
    assert (repo_uri, VCS.starCount, Literal(35000)) in g

    # Verify commit was created
    commit_triples = list(g.triples((None, RDF.type, VCS.Commit)))
    assert len(commit_triples) == 1

    # Verify upstream link goes through SourcePackage (domain constraint)
    upstream_triples = list(g.triples((src_uri, PKG.hasUpstreamProject, None)))
    assert len(upstream_triples) == 1, "SourcePackage should have upstream project link"

    # Verify upstream project links to repository
    upstream_uri = upstream_triples[0][2]
    assert (upstream_uri, VCS.hasUpstreamRepository, repo_uri) in g


@pytest.mark.unit
@patch("packagegraph.collectors.github.requests.get")
@patch("packagegraph.collectors.github.time.sleep")
def test_github_skips_non_github_urls(mock_sleep, mock_get):
    """GitHubEnricher should skip packages without GitHub homepage."""
    g = Graph()

    # Add package with non-GitHub homepage
    pkg_uri = DATA["package/debian/bookworm/amd64/nginx/1.24-1"]
    g.add((pkg_uri, RDF.type, PKG.BinaryPackage))
    g.add((pkg_uri, PKG.packageName, Literal("nginx")))
    g.add((pkg_uri, PKG.homepage, Literal("https://nginx.org")))

    enricher = GitHubEnricher(g, github_token="fake-token", cache_dir=None)
    with patch("packagegraph.collectors.github.click.echo"):
        enricher.enrich()

    # No API calls should be made
    assert mock_get.call_count == 0

    # No repositories should be created
    repo_triples = list(g.triples((None, RDF.type, VCS.Repository)))
    assert len(repo_triples) == 0

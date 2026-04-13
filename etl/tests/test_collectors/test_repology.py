import pytest
from rdflib import Graph, Literal
from rdflib.namespace import RDF
from unittest.mock import Mock, patch
from packagegraph.namespaces import PKG, DATA
from packagegraph.collectors.repology import RepologyEnricher


@pytest.mark.unit
@patch('packagegraph.collectors.repology.requests.get')
@patch('packagegraph.collectors.repology.time.sleep')  # Mock sleep to speed up tests
def test_repology_enrichment(mock_sleep, mock_get):
    """RepologyEnricher should add pkg:equivalentInDistribution links."""
    # Create a graph with packages from different distros
    g = Graph()

    # Add a Debian package
    debian_pkg = DATA["package/debian/bookworm/amd64/curl/8.4.0-2"]
    g.add((debian_pkg, RDF.type, PKG.BinaryPackage))
    g.add((debian_pkg, PKG.packageName, Literal("curl")))

    # Add a Fedora package
    fedora_pkg = DATA["package/fedora/41/x86_64/curl/8.6.0-5.fc41.x86_64"]
    g.add((fedora_pkg, RDF.type, PKG.BinaryPackage))
    g.add((fedora_pkg, PKG.packageName, Literal("curl")))

    # Mock repology API response
    repology_response = Mock()
    repology_response.json.return_value = [
        {
            "repo": "debian_12",
            "visiblename": "curl",
            "version": "8.4.0-2",
            "status": "newest"
        },
        {
            "repo": "fedora_41",
            "visiblename": "curl",
            "version": "8.6.0-5.fc41",
            "status": "newest"
        }
    ]
    repology_response.raise_for_status = Mock()
    mock_get.return_value = repology_response

    # Run enricher
    enricher = RepologyEnricher(g, cache_dir=None)  # No cache for test
    with patch('packagegraph.collectors.repology.click.echo'):
        enricher.enrich()

    # Verify cross-distro link was created
    equiv_links = list(g.triples((debian_pkg, PKG.equivalentInDistribution, fedora_pkg)))
    assert len(equiv_links) == 1, "Should create Debian→Fedora equivalence link"

    # Verify reverse link (symmetric property)
    reverse_links = list(g.triples((fedora_pkg, PKG.equivalentInDistribution, debian_pkg)))
    assert len(reverse_links) == 1, "Should create Fedora→Debian equivalence link"

    # Verify API was called for curl
    assert mock_get.called
    call_args = mock_get.call_args[0][0]
    assert "curl" in call_args


@pytest.mark.unit
@patch('packagegraph.collectors.repology.requests.get')
@patch('packagegraph.collectors.repology.time.sleep')
def test_repology_caching(mock_sleep, mock_get, tmp_path):
    """RepologyEnricher should cache API responses."""
    g = Graph()

    # Add a package
    debian_pkg = DATA["package/debian/bookworm/amd64/bash/5.2.15-2"]
    g.add((debian_pkg, RDF.type, PKG.BinaryPackage))
    g.add((debian_pkg, PKG.packageName, Literal("bash")))

    # Mock repology API response
    repology_response = Mock()
    repology_response.json.return_value = [
        {
            "repo": "debian_12",
            "visiblename": "bash",
            "version": "5.2.15-2",
            "status": "newest"
        }
    ]
    repology_response.raise_for_status = Mock()
    mock_get.return_value = repology_response

    cache_dir = tmp_path / "repology_cache"

    # First run - should call API
    enricher = RepologyEnricher(g, cache_dir=str(cache_dir))
    with patch('packagegraph.collectors.repology.click.echo'):
        enricher.enrich()

    assert mock_get.call_count == 1, "Should call API on first run"

    # Reset mock
    mock_get.reset_mock()

    # Second run - should use cache
    enricher2 = RepologyEnricher(g, cache_dir=str(cache_dir))
    with patch('packagegraph.collectors.repology.click.echo'):
        enricher2.enrich()

    assert mock_get.call_count == 0, "Should NOT call API on second run (use cache)"


@pytest.mark.unit
@patch('packagegraph.collectors.repology.requests.get')
@patch('packagegraph.collectors.repology.time.sleep')
def test_repology_rate_limiting(mock_sleep, mock_get):
    """RepologyEnricher should rate limit requests."""
    g = Graph()

    # Add multiple packages
    for pkg_name in ["curl", "bash", "vim"]:
        pkg_uri = DATA[f"package/debian/bookworm/amd64/{pkg_name}/1.0-1"]
        g.add((pkg_uri, RDF.type, PKG.BinaryPackage))
        g.add((pkg_uri, PKG.packageName, Literal(pkg_name)))

    # Mock repology API response
    mock_response = Mock()
    mock_response.json.return_value = []
    mock_response.raise_for_status = Mock()
    mock_get.return_value = mock_response

    # Run enricher
    enricher = RepologyEnricher(g, cache_dir=None)
    with patch('packagegraph.collectors.repology.click.echo'):
        enricher.enrich()

    # Verify sleep was called between requests (rate limiting)
    # With 3 packages, should sleep 2 times (not before first, but after each of first 2)
    assert mock_sleep.call_count >= 2, "Should sleep between API requests for rate limiting"

import pytest
from rdflib import Graph, Literal, URIRef
from rdflib.namespace import RDF, FOAF
from unittest.mock import Mock, patch
import requests
from packagegraph.namespaces import PKG, DEB, DATA
from packagegraph.collectors.debian import DebianCollector


@pytest.mark.unit
@patch('packagegraph.collectors.debian.requests.get')
def test_maintainer_parsing(mock_get):
    """Refactored collector should parse Maintainer field into pkg:Maintainer."""
    # Mock Release file
    release_response = Mock()
    release_response.text = "Codename: bookworm\nSuite: stable\nOrigin: Debian"
    release_response.raise_for_status = Mock()

    # Mock Packages.gz with maintainer
    packages_gz_content = b"""Package: curl
Version: 8.4.0-2
Architecture: amd64
Maintainer: Jane Doe <jane.doe@debian.org>
Description: command line tool
"""
    packages_response = Mock()
    packages_response.content = packages_gz_content
    packages_response.raise_for_status = Mock()

    # Mock Contents file - raise HTTPError to simulate 404
    mock_get.side_effect = [release_response, packages_response, requests.exceptions.HTTPError("404")]

    g = Graph()
    collector = DebianCollector(
        g=g,
        repo_url="http://deb.debian.org/debian",
        distribution="stable",
        component="main",
        arch="binary-amd64",
        parallel=False
    )

    with patch('packagegraph.collectors.debian.gzip.decompress', return_value=packages_gz_content):
        with patch('packagegraph.collectors.debian.click.echo'):  # Suppress warning output
            collector.collect()

    # Check maintainer was created
    maintainer_uri = DATA["maintainer/jane.doe@debian.org"]
    assert (maintainer_uri, RDF.type, PKG.Maintainer) in g
    assert (maintainer_uri, FOAF.name, Literal("Jane Doe")) in g
    assert (maintainer_uri, FOAF.mbox, URIRef("mailto:jane.doe@debian.org")) in g

    # Check package is linked to maintainer
    pkg_triples = list(g.triples((None, PKG.maintainedBy, maintainer_uri)))
    assert len(pkg_triples) == 1


@pytest.mark.unit
@patch('packagegraph.collectors.debian.requests.get')
def test_source_package_linking(mock_get):
    """Refactored collector should parse Source field and create SourcePackage."""
    release_response = Mock()
    release_response.text = "Codename: bookworm\nSuite: stable\nOrigin: Debian"
    release_response.raise_for_status = Mock()

    packages_gz_content = b"""Package: libcurl4
Version: 8.4.0-2
Architecture: amd64
Source: curl (8.4.0-2)
Maintainer: Jane Doe <jane@debian.org>
Description: library
"""
    packages_response = Mock()
    packages_response.content = packages_gz_content
    packages_response.raise_for_status = Mock()

    # Mock Contents file - raise HTTPError to simulate 404
    mock_get.side_effect = [release_response, packages_response, requests.exceptions.HTTPError("404")]

    g = Graph()
    collector = DebianCollector(
        g=g,
        repo_url="http://deb.debian.org/debian",
        distribution="stable",
        component="main",
        arch="binary-amd64",
        parallel=False
    )

    with patch('packagegraph.collectors.debian.gzip.decompress', return_value=packages_gz_content):
        with patch('packagegraph.collectors.debian.click.echo'):  # Suppress warning output
            collector.collect()

    # Check source package was created
    src_pkg_uri = DATA["source/debian/bookworm/curl/8.4.0-2"]
    assert (src_pkg_uri, RDF.type, PKG.SourcePackage) in g
    assert (src_pkg_uri, PKG.packageName, Literal("curl")) in g

    # Check binary package is linked to source
    bin_pkg_triples = list(g.triples((None, PKG.builtFromSource, src_pkg_uri)))
    assert len(bin_pkg_triples) == 1


@pytest.mark.unit
@patch('packagegraph.collectors.debian.requests.get')
def test_dual_typing(mock_get):
    """Refactored collector should emit both pkg:BinaryPackage and deb:BinaryPackage."""
    release_response = Mock()
    release_response.text = "Codename: bookworm\nSuite: stable\nOrigin: Debian"
    release_response.raise_for_status = Mock()

    packages_gz_content = b"""Package: curl
Version: 8.4.0-2
Architecture: amd64
Maintainer: Jane Doe <jane@debian.org>
Description: tool
"""
    packages_response = Mock()
    packages_response.content = packages_gz_content
    packages_response.raise_for_status = Mock()

    # Mock Contents file - raise HTTPError to simulate 404
    mock_get.side_effect = [release_response, packages_response, requests.exceptions.HTTPError("404")]

    g = Graph()
    collector = DebianCollector(
        g=g,
        repo_url="http://deb.debian.org/debian",
        distribution="stable",
        component="main",
        arch="binary-amd64",
        parallel=False
    )

    with patch('packagegraph.collectors.debian.gzip.decompress', return_value=packages_gz_content):
        with patch('packagegraph.collectors.debian.click.echo'):  # Suppress warning output
            collector.collect()

    # Check dual typing
    pkg_triples = list(g.triples((None, RDF.type, PKG.BinaryPackage)))
    assert len(pkg_triples) == 1
    pkg_uri = pkg_triples[0][0]

    assert (pkg_uri, RDF.type, DEB.BinaryPackage) in g


@pytest.mark.unit
@patch('packagegraph.collectors.debian.requests.get')
def test_multi_arch_collection(mock_get):
    """Refactored collector should handle multiple architectures."""
    release_response = Mock()
    release_response.text = "Codename: bookworm\nSuite: stable\nOrigin: Debian"
    release_response.raise_for_status = Mock()

    # Mock Packages.gz for amd64
    packages_amd64 = b"""Package: curl-amd64
Version: 8.4.0-2
Architecture: amd64
Maintainer: Jane Doe <jane@debian.org>
Description: curl for amd64
"""

    # Mock Packages.gz for arm64
    packages_arm64 = b"""Package: curl-arm64
Version: 8.4.0-2
Architecture: arm64
Maintainer: Jane Doe <jane@debian.org>
Description: curl for arm64
"""

    packages_amd64_response = Mock()
    packages_amd64_response.content = packages_amd64
    packages_amd64_response.raise_for_status = Mock()

    packages_arm64_response = Mock()
    packages_arm64_response.content = packages_arm64
    packages_arm64_response.raise_for_status = Mock()

    # Mock Contents files (404)
    mock_get.side_effect = [
        release_response,
        packages_amd64_response,
        requests.exceptions.HTTPError("404"),  # Contents-amd64.gz
        packages_arm64_response,
        requests.exceptions.HTTPError("404"),  # Contents-arm64.gz
    ]

    g = Graph()
    collector = DebianCollector(
        g=g,
        repo_url="http://deb.debian.org/debian",
        distribution="stable",
        component="main",
        arch=["binary-amd64", "binary-arm64"],  # Multiple architectures
        parallel=False
    )

    with patch('packagegraph.collectors.debian.gzip.decompress') as mock_decompress:
        # Return appropriate content based on call count
        mock_decompress.side_effect = [packages_amd64, packages_arm64]
        with patch('packagegraph.collectors.debian.click.echo'):
            collector.collect()

    # Check that packages for both architectures were created
    amd64_packages = list(g.triples((None, PKG.targetArchitecture, DATA["arch/amd64"])))
    arm64_packages = list(g.triples((None, PKG.targetArchitecture, DATA["arch/arm64"])))

    assert len(amd64_packages) == 1, "Should have one amd64 package"
    assert len(arm64_packages) == 1, "Should have one arm64 package"

    # Verify distinct package URIs
    amd64_pkg_uri = amd64_packages[0][0]
    arm64_pkg_uri = arm64_packages[0][0]
    assert amd64_pkg_uri != arm64_pkg_uri, "Packages should have distinct URIs"

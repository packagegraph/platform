"""Integration tests for the ETL pipeline with SPARQL verification."""

import pytest
from rdflib import Graph
from unittest.mock import Mock, patch
from packagegraph.collectors.debian import DebianCollector
from packagegraph.collectors.rpm import RpmCollector


@pytest.mark.integration
def test_debian_rpm_integration_with_sparql():
    """
    Integration test: Debian + RPM collectors → merged graph → SPARQL queries.

    Verifies:
    1. pkg:BinaryPackage count
    2. Dependency links (pkg:directlyDependsOn)
    3. Maintainer aggregation
    4. Source→Binary links
    5. Dual typing (deb:BinaryPackage, rpm:BinaryRPM)
    """
    g = Graph()

    # Mock Debian data
    debian_release_response = Mock()
    debian_release_response.text = "Codename: bookworm\nSuite: stable\nOrigin: Debian"
    debian_release_response.raise_for_status = Mock()

    debian_packages = b"""Package: curl
Version: 8.4.0-2
Architecture: amd64
Maintainer: Alice Developer <alice@debian.org>
Depends: libc6, libcurl4
Source: curl
Description: command line tool for transferring data

Package: libcurl4
Version: 8.4.0-2
Architecture: amd64
Maintainer: Alice Developer <alice@debian.org>
Source: curl (8.4.0-2)
Description: library for URL transfers
"""

    debian_packages_response = Mock()
    debian_packages_response.content = debian_packages
    debian_packages_response.raise_for_status = Mock()

    # Mock RPM data
    rpm_repomd_response = Mock()
    rpm_repomd_response.content = b"""<?xml version="1.0"?>
<repomd xmlns="http://linux.duke.edu/metadata/repo">
  <data type="primary">
    <location href="repodata/primary.xml.gz"/>
  </data>
</repomd>"""
    rpm_repomd_response.raise_for_status = Mock()

    rpm_primary_xml = b"""<?xml version="1.0"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm">
  <package type="rpm">
    <name>bash</name>
    <arch>x86_64</arch>
    <version epoch="0" ver="5.2.15" rel="1.fc41"/>
    <checksum type="sha256">abc123</checksum>
    <summary>The GNU Bourne Again shell</summary>
    <description>Bash is the shell</description>
    <format>
      <rpm:license>GPLv3+</rpm:license>
    </format>
  </package>
  <package type="rpm">
    <name>vim</name>
    <arch>x86_64</arch>
    <version epoch="0" ver="9.1.0" rel="1.fc41"/>
    <checksum type="sha256">def456</checksum>
    <summary>The VIM editor</summary>
    <description>Vi IMproved</description>
    <format>
      <rpm:license>Vim</rpm:license>
    </format>
  </package>
</metadata>"""

    rpm_primary_response = Mock()
    rpm_primary_response.content = rpm_primary_xml
    rpm_primary_response.raise_for_status = Mock()

    with patch('packagegraph.collectors.debian.requests.get') as debian_mock:
        # Debian: Release, Packages, Contents (404)
        import requests
        debian_mock.side_effect = [
            debian_release_response,
            debian_packages_response,
            requests.exceptions.HTTPError("404")
        ]

        with patch('packagegraph.collectors.debian.gzip.decompress', return_value=debian_packages):
            with patch('packagegraph.collectors.debian.click.echo'):
                debian_collector = DebianCollector(
                    g,
                    "http://deb.debian.org/debian",
                    distribution="stable",
                    component="main",
                    arch=["binary-amd64"],
                    parallel=False
                )
                debian_collector.collect()

    with patch('packagegraph.collectors.rpm.requests.get') as rpm_mock:
        rpm_mock.side_effect = [rpm_repomd_response, rpm_primary_response]

        with patch('packagegraph.collectors.rpm.gzip.decompress', return_value=rpm_primary_xml):
            with patch('packagegraph.collectors.rpm.click.echo'):
                rpm_collector = RpmCollector(
                    g,
                    "https://dl.fedoraproject.org/pub/fedora/linux/releases/41/Everything/x86_64/os",
                    distro_name="fedora",
                    release_name="41",
                    parallel=False
                )
                rpm_collector.collect()

    # === SPARQL Verification ===

    # Query 1: Count all BinaryPackages (should be 4: 2 Debian + 2 RPM)
    query1 = """
        PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
        SELECT (COUNT(?p) as ?count) WHERE {
            ?p a pkg:BinaryPackage .
        }
    """
    results = g.query(query1)
    result_list = list(results)
    assert len(result_list) == 1
    count = int(result_list[0][0])
    assert count == 4, f"Expected 4 BinaryPackages, got {count}"

    # Query 2: Check dependency links (Debian curl → libc6, libcurl4)
    # NOTE: Dependency processing is implemented but may create named nodes for deps
    # that don't exist in the fixture. Just verify the relationship exists.
    query2 = """
        PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
        SELECT (COUNT(?dep) as ?count) WHERE {
            ?p pkg:packageName "curl" .
            ?p pkg:directlyDependsOn ?dep .
        }
    """
    results = g.query(query2)
    result_list = list(results)
    dep_count = int(result_list[0][0])
    assert dep_count >= 2, f"curl should have at least 2 dependencies, got {dep_count}"

    # Query 3: Check maintainer aggregation (Alice should maintain 2 Debian packages)
    query3 = """
        PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
        PREFIX foaf: <http://xmlns.com/foaf/0.1/>
        SELECT ?name (COUNT(DISTINCT ?p) as ?count) WHERE {
            ?p pkg:maintainedBy ?m .
            ?m foaf:name ?name .
        }
        GROUP BY ?name
    """
    results = g.query(query3)
    result_list = list(results)
    assert len(result_list) == 1, "Should have one maintainer"
    name, count = result_list[0]
    assert str(name) == "Alice Developer"
    assert int(count) == 2, f"Alice should maintain 2 packages, got {count}"

    # Query 4: Check source→binary links
    query4 = """
        PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
        SELECT ?bin_name ?src_name WHERE {
            ?bin pkg:builtFromSource ?src .
            ?bin pkg:packageName ?bin_name .
            ?src pkg:packageName ?src_name .
        }
    """
    results = g.query(query4)
    source_links = {(str(bin_name), str(src_name)) for bin_name, src_name in results}
    assert ("curl", "curl") in source_links
    assert ("libcurl4", "curl") in source_links

    # Query 5: Verify dual typing - Debian packages
    query5 = """
        PREFIX deb: <https://packagegraph.github.io/ontology/debian#>
        SELECT (COUNT(?p) as ?count) WHERE {
            ?p a deb:BinaryPackage .
        }
    """
    results = g.query(query5)
    result_list = list(results)
    deb_count = int(result_list[0][0])
    assert deb_count == 2, f"Should have 2 deb:BinaryPackage, got {deb_count}"

    # Query 6: Verify dual typing - RPM packages
    query6 = """
        PREFIX rpm: <https://packagegraph.github.io/ontology/rpm#>
        SELECT (COUNT(?p) as ?count) WHERE {
            ?p a rpm:BinaryRPM .
        }
    """
    results = g.query(query6)
    result_list = list(results)
    rpm_count = int(result_list[0][0])
    assert rpm_count == 2, f"Should have 2 rpm:BinaryRPM, got {rpm_count}"

    # Query 7: Verify all BinaryPackages also have distro-specific type
    query7 = """
        PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
        PREFIX deb: <https://packagegraph.github.io/ontology/debian#>
        PREFIX rpm: <https://packagegraph.github.io/ontology/rpm#>
        SELECT (COUNT(?p) as ?count) WHERE {
            ?p a pkg:BinaryPackage .
            FILTER NOT EXISTS {
                { ?p a deb:BinaryPackage } UNION { ?p a rpm:BinaryRPM }
            }
        }
    """
    results = g.query(query7)
    result_list = list(results)
    untyped_count = int(result_list[0][0])
    assert untyped_count == 0, f"All BinaryPackages should have distro-specific type, {untyped_count} don't"

    print(f"\n✅ Integration test passed - verified {len(g)} triples via SPARQL")


@pytest.mark.integration
def test_multi_arch_integration():
    """Verify multi-arch collection creates distinct packages per architecture."""
    g = Graph()

    debian_release_response = Mock()
    debian_release_response.text = "Codename: bookworm\nSuite: stable\nOrigin: Debian"
    debian_release_response.raise_for_status = Mock()

    # Same package, different architectures
    packages_amd64 = b"""Package: gcc
Version: 13.2.0-1
Architecture: amd64
Maintainer: GCC Team <gcc@debian.org>
Description: GNU C compiler
"""

    packages_arm64 = b"""Package: gcc
Version: 13.2.0-1
Architecture: arm64
Maintainer: GCC Team <gcc@debian.org>
Description: GNU C compiler
"""

    packages_amd64_response = Mock()
    packages_amd64_response.content = packages_amd64
    packages_amd64_response.raise_for_status = Mock()

    packages_arm64_response = Mock()
    packages_arm64_response.content = packages_arm64
    packages_arm64_response.raise_for_status = Mock()

    import requests
    with patch('packagegraph.collectors.debian.requests.get') as mock_get:
        mock_get.side_effect = [
            debian_release_response,
            packages_amd64_response,
            requests.exceptions.HTTPError("404"),  # Contents-amd64
            packages_arm64_response,
            requests.exceptions.HTTPError("404"),  # Contents-arm64
        ]

        with patch('packagegraph.collectors.debian.gzip.decompress') as mock_decompress:
            mock_decompress.side_effect = [packages_amd64, packages_arm64]
            with patch('packagegraph.collectors.debian.click.echo'):
                collector = DebianCollector(
                    g,
                    "http://deb.debian.org/debian",
                    distribution="stable",
                    component="main",
                    arch=["binary-amd64", "binary-arm64"],
                    parallel=False
                )
                collector.collect()

    # SPARQL: Verify distinct packages per architecture
    query = """
        PREFIX pkg: <https://packagegraph.github.io/ontology/core#>
        PREFIX data: <https://packagegraph.github.io/data/>
        SELECT ?arch (COUNT(?p) as ?count) WHERE {
            ?p pkg:packageName "gcc" .
            ?p pkg:targetArchitecture ?arch_uri .
            ?arch_uri a pkg:Architecture .
            BIND(STRAFTER(STR(?arch_uri), "/arch/") AS ?arch)
        }
        GROUP BY ?arch
    """
    results = g.query(query)
    arch_counts = {str(arch): int(count) for arch, count in results}

    assert arch_counts.get("amd64") == 1, "Should have 1 gcc for amd64"
    assert arch_counts.get("arm64") == 1, "Should have 1 gcc for arm64"

    print("\n✅ Multi-arch integration test passed")

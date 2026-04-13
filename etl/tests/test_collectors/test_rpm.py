import pytest
from rdflib import Graph, Literal
from rdflib.namespace import RDF
from unittest.mock import Mock, patch
from packagegraph.namespaces import PKG, RPM, DATA
from packagegraph.collectors.rpm import RpmCollector


@pytest.mark.unit
@patch('packagegraph.collectors.rpm.requests.get')
def test_rpm_dual_typing(mock_get):
    """RPM collector should emit both pkg:BinaryPackage and rpm:BinaryRPM."""
    # Mock repomd.xml
    repomd_response = Mock()
    repomd_response.content = b"""<?xml version="1.0"?>
<repomd xmlns="http://linux.duke.edu/metadata/repo">
  <data type="primary">
    <location href="repodata/primary.xml.gz"/>
  </data>
</repomd>"""
    repomd_response.raise_for_status = Mock()

    # Mock primary.xml.gz
    primary_xml = b"""<?xml version="1.0"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm">
  <package type="rpm">
    <name>curl</name>
    <arch>x86_64</arch>
    <version epoch="0" ver="8.6.0" rel="5.fc41"/>
    <checksum type="sha256">abc123</checksum>
    <summary>A utility for getting files from remote servers</summary>
    <description>curl is a command line tool</description>
    <format>
      <rpm:license>MIT</rpm:license>
      <rpm:vendor>Fedora Project</rpm:vendor>
      <rpm:group>Applications/Internet</rpm:group>
      <rpm:sourcerpm>curl-8.6.0-5.fc41.src.rpm</rpm:sourcerpm>
    </format>
  </package>
</metadata>"""
    primary_response = Mock()
    primary_response.content = primary_xml
    primary_response.raise_for_status = Mock()

    mock_get.side_effect = [repomd_response, primary_response]

    g = Graph()
    collector = RpmCollector(
        g=g,
        repo_url="https://dl.fedoraproject.org/pub/fedora/linux/releases/41/Everything/x86_64/os",
        distro_name="fedora",
        release_name="41",
        parallel=False
    )

    with patch('packagegraph.collectors.rpm.gzip.decompress', return_value=primary_xml):
        with patch('packagegraph.collectors.rpm.click.echo'):
            collector.collect()

    # Check dual typing
    pkg_triples = list(g.triples((None, RDF.type, PKG.BinaryPackage)))
    assert len(pkg_triples) == 1
    pkg_uri = pkg_triples[0][0]

    assert (pkg_uri, RDF.type, RPM.BinaryRPM) in g
    assert (pkg_uri, PKG.packageName, Literal("curl")) in g


@pytest.mark.unit
@patch('packagegraph.collectors.rpm.requests.get')
def test_multi_release_rpm_collection(mock_get):
    """Multi-release RPM collection should create distinct packages per release."""
    # Mock responses for Fedora 41
    repomd_41 = Mock()
    repomd_41.content = b"""<?xml version="1.0"?>
<repomd xmlns="http://linux.duke.edu/metadata/repo">
  <data type="primary">
    <location href="repodata/primary.xml.gz"/>
  </data>
</repomd>"""
    repomd_41.raise_for_status = Mock()

    primary_41 = b"""<?xml version="1.0"?>
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
</metadata>"""

    # Mock responses for Fedora 42
    repomd_42 = Mock()
    repomd_42.content = b"""<?xml version="1.0"?>
<repomd xmlns="http://linux.duke.edu/metadata/repo">
  <data type="primary">
    <location href="repodata/primary.xml.gz"/>
  </data>
</repomd>"""
    repomd_42.raise_for_status = Mock()

    primary_42 = b"""<?xml version="1.0"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm">
  <package type="rpm">
    <name>bash</name>
    <arch>x86_64</arch>
    <version epoch="0" ver="5.2.21" rel="1.fc42"/>
    <checksum type="sha256">def456</checksum>
    <summary>The GNU Bourne Again shell</summary>
    <description>Bash is the shell</description>
    <format>
      <rpm:license>GPLv3+</rpm:license>
    </format>
  </package>
</metadata>"""

    # Mock get calls: repomd_41, primary_41, repomd_42, primary_42
    mock_get.side_effect = [repomd_41, repomd_41, repomd_42, repomd_42]

    g = Graph()

    # Collect from Fedora 41
    collector_41 = RpmCollector(
        g=g,
        repo_url="https://dl.fedoraproject.org/pub/fedora/linux/releases/41/Everything/x86_64/os",
        distro_name="fedora",
        release_name="41",
        parallel=False
    )

    with patch('packagegraph.collectors.rpm.gzip.decompress', return_value=primary_41):
        with patch('packagegraph.collectors.rpm.click.echo'):
            collector_41.collect()

    # Collect from Fedora 42
    collector_42 = RpmCollector(
        g=g,
        repo_url="https://dl.fedoraproject.org/pub/fedora/linux/releases/42/Everything/x86_64/os",
        distro_name="fedora",
        release_name="42",
        parallel=False
    )

    with patch('packagegraph.collectors.rpm.gzip.decompress', return_value=primary_42):
        with patch('packagegraph.collectors.rpm.click.echo'):
            collector_42.collect()

    # Check that we have two distinct packages (different versions/releases)
    pkg_triples = list(g.triples((None, RDF.type, PKG.BinaryPackage)))
    assert len(pkg_triples) == 2, "Should have two packages (one per release)"

    # Verify distinct URIs based on version difference
    pkg_uris = [triple[0] for triple in pkg_triples]
    assert len(set(pkg_uris)) == 2, "Package URIs should be distinct"

    # Verify both have the same name but different releases
    bash_packages = list(g.triples((None, PKG.packageName, Literal("bash"))))
    assert len(bash_packages) == 2, "Both packages should be named bash"

    # Verify distinct releases
    release_41_pkg = DATA["package/fedora/41/x86_64/bash/5.2.15-1.fc41.x86_64"]
    release_42_pkg = DATA["package/fedora/42/x86_64/bash/5.2.21-1.fc42.x86_64"]

    assert (release_41_pkg, RDF.type, PKG.BinaryPackage) in g
    assert (release_42_pkg, RDF.type, PKG.BinaryPackage) in g

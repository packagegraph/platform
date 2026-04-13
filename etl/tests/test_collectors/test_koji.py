import pytest
from rdflib import Graph, Literal
from rdflib.namespace import RDF
from unittest.mock import patch, MagicMock
from packagegraph.namespaces import PKG, RPM, PROV, DATA
from packagegraph.collectors.koji import KojiEnricher


@pytest.mark.unit
@patch('packagegraph.collectors.koji.xmlrpc.client.ServerProxy')
@patch('packagegraph.collectors.koji.time.sleep')
def test_koji_build_metadata(mock_sleep, mock_proxy_class):
    """KojiEnricher should create pkg:BuildActivity linked to RPM packages."""
    g = Graph()

    # Add an RPM package
    pkg_uri = DATA["package/fedora/41/x86_64/bash/5.2.15-1.fc41.x86_64"]
    ver_uri = DATA["version/fedora/41/bash/5.2.15-1.fc41.x86_64"]
    g.add((pkg_uri, RDF.type, PKG.BinaryPackage))
    g.add((pkg_uri, RDF.type, RPM.BinaryRPM))
    g.add((pkg_uri, PKG.packageName, Literal("bash")))
    g.add((pkg_uri, PKG.hasVersion, ver_uri))
    g.add((ver_uri, RDF.type, PKG.Version))
    g.add((ver_uri, PKG.versionString, Literal("5.2.15-1.fc41.x86_64")))

    # Mock koji XML-RPC client
    mock_proxy = MagicMock()
    mock_proxy_class.return_value = mock_proxy

    # Mock getBuild response
    mock_proxy.getBuild.return_value = {
        "build_id": 12345,
        "name": "bash",
        "version": "5.2.15",
        "release": "1.fc41",
        "owner_name": "packager1",
        "start_time": "2024-01-10 08:00:00",
        "completion_time": "2024-01-10 08:15:00",
    }

    # Mock listBuildRPMs response (build dependencies)
    mock_proxy.listBuildRPMs.return_value = [
        {"name": "gcc", "version": "13.2.0", "release": "1.fc41", "arch": "x86_64"},
        {"name": "glibc", "version": "2.38", "release": "1.fc41", "arch": "x86_64"},
    ]

    enricher = KojiEnricher(
        g,
        koji_hub="https://koji.fedoraproject.org/kojihub",
        distro_name="fedora",
        release_name="41",
        cache_dir=None
    )
    with patch('packagegraph.collectors.koji.click.echo'):
        enricher.enrich()

    # Verify BuildActivity was created
    build_triples = list(g.triples((None, RDF.type, PKG.BuildActivity)))
    assert len(build_triples) == 1

    build_uri = build_triples[0][0]
    assert (build_uri, RDF.type, PROV.Activity) in g
    assert (build_uri, PKG.wasBuiltBy, Literal("packager1")) in g

    # Verify package linked to build
    produced_triples = list(g.triples((pkg_uri, PKG.wasProducedBy, build_uri)))
    assert len(produced_triples) == 1

    # Verify build dependencies were recorded
    dep_triples = list(g.triples((build_uri, PKG.usedDependency, None)))
    assert len(dep_triples) == 2, "Build should have 2 dependencies (gcc, glibc)"


@pytest.mark.unit
@patch('packagegraph.collectors.koji.xmlrpc.client.ServerProxy')
@patch('packagegraph.collectors.koji.time.sleep')
def test_koji_build_not_found(mock_sleep, mock_proxy_class):
    """KojiEnricher should handle packages not found in koji."""
    g = Graph()

    pkg_uri = DATA["package/fedora/41/x86_64/unknown-pkg/1.0-1.fc41.x86_64"]
    g.add((pkg_uri, RDF.type, PKG.BinaryPackage))
    g.add((pkg_uri, RDF.type, RPM.BinaryRPM))
    g.add((pkg_uri, PKG.packageName, Literal("unknown-pkg")))

    mock_proxy = MagicMock()
    mock_proxy_class.return_value = mock_proxy
    mock_proxy.getBuild.return_value = None

    enricher = KojiEnricher(
        g,
        koji_hub="https://koji.fedoraproject.org/kojihub",
        distro_name="fedora",
        release_name="41",
        cache_dir=None
    )
    with patch('packagegraph.collectors.koji.click.echo'):
        enricher.enrich()

    # No build activities should be created
    build_triples = list(g.triples((None, RDF.type, PKG.BuildActivity)))
    assert len(build_triples) == 0

import pytest
from rdflib import Graph, Literal
from rdflib.namespace import RDF
from unittest.mock import Mock, patch
from packagegraph.namespaces import PKG, SEC, DATA
from packagegraph.collectors.security import SecurityEnricher


@pytest.mark.unit
@patch("packagegraph.collectors.security.requests.get")
@patch("packagegraph.collectors.security.time.sleep")
def test_security_enrichment(mock_sleep, mock_get):
    """SecurityEnricher should create sec:Vulnerability and link to versions."""
    g = Graph()

    # Add a package with version
    pkg_uri = DATA["package/debian/bookworm/amd64/curl/8.4.0-2"]
    ver_uri = DATA["version/debian/bookworm/curl/8.4.0-2"]
    g.add((pkg_uri, RDF.type, PKG.BinaryPackage))
    g.add((pkg_uri, PKG.packageName, Literal("curl")))
    g.add((pkg_uri, PKG.hasVersion, ver_uri))
    g.add((ver_uri, RDF.type, PKG.Version))
    g.add((ver_uri, PKG.versionString, Literal("8.4.0-2")))

    # Mock OSV API response
    osv_response = Mock()
    osv_response.json.return_value = {
        "vulns": [
            {
                "id": "CVE-2024-1234",
                "summary": "Buffer overflow in curl",
                "severity": [{"type": "CVSS_V3", "score": "7.5"}],
                "published": "2024-01-15T00:00:00Z",
                "modified": "2024-02-01T00:00:00Z",
                "affected": [
                    {
                        "package": {"name": "curl", "ecosystem": "Debian"},
                        "ranges": [
                            {
                                "type": "ECOSYSTEM",
                                "events": [{"introduced": "7.0.0"}, {"fixed": "8.5.0"}],
                            }
                        ],
                    }
                ],
            }
        ]
    }
    osv_response.raise_for_status = Mock()
    osv_response.status_code = 200

    mock_get.return_value = osv_response

    enricher = SecurityEnricher(g, cache_dir=None)
    with patch("packagegraph.collectors.security.click.echo"):
        enricher.enrich()

    # Verify vulnerability was created
    vuln_triples = list(g.triples((None, RDF.type, SEC.Vulnerability)))
    assert len(vuln_triples) == 1

    vuln_uri = vuln_triples[0][0]
    assert (vuln_uri, SEC.cveId, Literal("CVE-2024-1234")) in g
    assert (vuln_uri, SEC.severity, Literal("7.5")) in g

    # Verify version link
    affects_triples = list(g.triples((vuln_uri, SEC.affectsVersion, None)))
    assert len(affects_triples) == 1, "Vulnerability should link to affected version"


@pytest.mark.unit
@patch("packagegraph.collectors.security.requests.get")
@patch("packagegraph.collectors.security.time.sleep")
def test_security_no_vulns(mock_sleep, mock_get):
    """SecurityEnricher should handle packages with no vulnerabilities."""
    g = Graph()

    pkg_uri = DATA["package/debian/bookworm/amd64/hello/2.10-1"]
    ver_uri = DATA["version/debian/bookworm/hello/2.10-1"]
    g.add((pkg_uri, RDF.type, PKG.BinaryPackage))
    g.add((pkg_uri, PKG.packageName, Literal("hello")))
    g.add((pkg_uri, PKG.hasVersion, ver_uri))
    g.add((ver_uri, RDF.type, PKG.Version))

    osv_response = Mock()
    osv_response.json.return_value = {"vulns": []}
    osv_response.raise_for_status = Mock()
    osv_response.status_code = 200
    mock_get.return_value = osv_response

    enricher = SecurityEnricher(g, cache_dir=None)
    with patch("packagegraph.collectors.security.click.echo"):
        enricher.enrich()

    vuln_triples = list(g.triples((None, RDF.type, SEC.Vulnerability)))
    assert len(vuln_triples) == 0

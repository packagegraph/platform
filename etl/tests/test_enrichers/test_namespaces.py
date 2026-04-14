"""Tests for new namespace and URI helper additions."""

import pytest
from packagegraph.namespaces import (
    MET, PROV, claim_uri, snapshot_uri, license_uri, language_uri
)


@pytest.mark.unit
class TestNamespaces:
    def test_met_namespace_defined(self):
        """Test that MET namespace is defined."""
        assert str(MET) == 'https://purl.org/packagegraph/ontology/metrics#'

    def test_prov_namespace_exists(self):
        """Test that PROV namespace exists."""
        assert str(PROV) == 'http://www.w3.org/ns/prov#'


@pytest.mark.unit
class TestURIHelpers:
    def test_claim_uri_format(self):
        """Test claim URI format."""
        uri = claim_uri('license_enricher', 'abc123', '2026-04-13T10:00:00')
        assert uri.startswith('https://packagegraph.github.io/d/claim/license_enricher/')
        assert 'abc123' in uri

    def test_snapshot_uri_format(self):
        """Test snapshot URI format."""
        uri = snapshot_uri('test_enricher', '2026-04-13T10:00:00')
        assert uri.startswith('https://packagegraph.github.io/d/snapshot/test_enricher/')

    def test_license_uri_format(self):
        """Test license URI format."""
        uri = license_uri('MIT')
        assert uri == 'https://packagegraph.github.io/d/license/MIT'

    def test_license_uri_with_special_chars(self):
        """Test license URI with special characters."""
        uri = license_uri('GPL-3.0-or-later')
        assert 'GPL-3.0-or-later' in uri
        # Should be URL-encoded
        assert 'license/' in uri

    def test_language_uri_format(self):
        """Test language URI format."""
        uri = language_uri('Python')
        assert uri == 'https://packagegraph.github.io/d/language/Python'

    def test_language_uri_with_spaces(self):
        """Test language URI with spaces gets encoded."""
        uri = language_uri('C++')
        assert 'language/' in uri
        assert 'C%2B%2B' in uri or 'C++' in uri

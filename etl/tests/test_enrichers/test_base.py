"""Tests for BaseEnricher and NTriplesWriter."""

import pytest
from io import StringIO
from unittest.mock import MagicMock, patch
from packagegraph.enrichers.base import BaseEnricher, NTriplesWriter


@pytest.mark.unit
class TestNTriplesWriter:
    def test_streaming_writes_directly_to_file(self):
        """Test that NTriplesWriter streams directly to file without accumulation."""
        output = StringIO()
        writer = NTriplesWriter(output)

        writer.write_lit('http://example.org/s1', 'http://example.org/p', 'value1')
        writer.write_uri('http://example.org/s2', 'http://example.org/p', 'http://example.org/o')

        # Content should be written immediately, not accumulated
        content = output.getvalue()
        assert '<http://example.org/s1> <http://example.org/p> "value1" .\n' in content
        assert '<http://example.org/s2> <http://example.org/p> <http://example.org/o> .\n' in content

    def test_escape_nt_basic(self):
        """Test N-Triples string escaping."""
        output = StringIO()
        writer = NTriplesWriter(output)
        assert writer._escape_nt('simple') == 'simple'
        assert writer._escape_nt('with "quotes"') == 'with \\"quotes\\"'
        assert writer._escape_nt('with\\backslash') == 'with\\\\backslash'
        assert writer._escape_nt('line\nbreak') == 'line\\nbreak'
        assert writer._escape_nt('tab\there') == 'tab\\there'

    def test_write_lit(self):
        """Test literal triple writing."""
        output = StringIO()
        writer = NTriplesWriter(output)
        writer.write_lit('http://example.org/s', 'http://example.org/p', 'value')
        content = output.getvalue()
        assert content == '<http://example.org/s> <http://example.org/p> "value" .\n'

    def test_write_int(self):
        """Test integer triple writing."""
        output = StringIO()
        writer = NTriplesWriter(output)
        writer.write_int('http://example.org/s', 'http://example.org/p', 42)
        content = output.getvalue()
        assert content == '<http://example.org/s> <http://example.org/p> "42"^^<http://www.w3.org/2001/XMLSchema#integer> .\n'

    def test_write_uri(self):
        """Test URI triple writing."""
        output = StringIO()
        writer = NTriplesWriter(output)
        writer.write_uri('http://example.org/s', 'http://example.org/p', 'http://example.org/o')
        content = output.getvalue()
        assert content == '<http://example.org/s> <http://example.org/p> <http://example.org/o> .\n'


@pytest.mark.unit
class TestBaseEnricher:
    def test_cannot_instantiate_abstract_class(self):
        """Test that BaseEnricher cannot be instantiated directly."""
        with pytest.raises(TypeError, match="Can't instantiate abstract class"):
            BaseEnricher(
                sparql_client=MagicMock(),
                output_path='/tmp/test.nt',
                enricher_name='test',
                enricher_version='1.0.0'
            )

    def test_concrete_enricher_lifecycle(self, tmp_path):
        """Test the full enricher lifecycle with a concrete implementation."""
        # Create a concrete implementation for testing
        class TestEnricher(BaseEnricher):
            def _query_packages(self):
                return [('pkg1', 'http://example.org/pkg1'), ('pkg2', 'http://example.org/pkg2')]

            def _process_item(self, item):
                name, uri = item
                self.writer.write_lit(uri, 'http://example.org/name', name)

        output_file = tmp_path / 'output.nt'
        mock_client = MagicMock()
        mock_client.query.return_value = []  # Mock Fuseki recency check

        enricher = TestEnricher(
            sparql_client=mock_client,
            output_path=str(output_file),
            enricher_name='test_enricher',
            enricher_version='1.0.0'
        )

        # Mock the recency validation to pass
        with patch.object(enricher, '_validate_fuseki_recency'):
            enricher.enrich()

        # Verify output file exists and has sorted content
        assert output_file.exists()
        content = output_file.read_text()
        lines = [line for line in content.split('\n') if line.strip()]
        # Now includes data triples (2) + provenance triples (8)
        assert len(lines) >= 2
        # Data triples should be sorted and present
        data_lines = [line for line in lines if 'http://example.org/name' in line]
        assert len(data_lines) == 2
        assert 'pkg1' in data_lines[0]
        assert 'pkg2' in data_lines[1]
        # Provenance triples should be present
        assert any('prov#Activity' in line for line in lines)
        assert any('DataSnapshot' in line for line in lines)


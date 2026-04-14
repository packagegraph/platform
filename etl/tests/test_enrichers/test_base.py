"""Tests for BaseEnricher and NTriplesWriter."""

import pytest
from io import StringIO
from unittest.mock import MagicMock, patch
from packagegraph.enrichers.base import BaseEnricher, NTriplesWriter


@pytest.mark.unit
class TestNTriplesWriter:
    def test_escape_nt_basic(self):
        """Test N-Triples string escaping."""
        writer = NTriplesWriter()
        assert writer._escape_nt('simple') == 'simple'
        assert writer._escape_nt('with "quotes"') == 'with \\"quotes\\"'
        assert writer._escape_nt('with\\backslash') == 'with\\\\backslash'
        assert writer._escape_nt('line\nbreak') == 'line\\nbreak'
        assert writer._escape_nt('tab\there') == 'tab\\there'

    def test_write_lit(self):
        """Test literal triple writing."""
        writer = NTriplesWriter()
        writer.write_lit('http://example.org/s', 'http://example.org/p', 'value')
        triples = writer.get_sorted_triples()
        assert len(triples) == 1
        assert triples[0] == '<http://example.org/s> <http://example.org/p> "value" .\n'

    def test_write_int(self):
        """Test integer triple writing."""
        writer = NTriplesWriter()
        writer.write_int('http://example.org/s', 'http://example.org/p', 42)
        triples = writer.get_sorted_triples()
        assert len(triples) == 1
        assert triples[0] == '<http://example.org/s> <http://example.org/p> "42"^^<http://www.w3.org/2001/XMLSchema#integer> .\n'

    def test_write_uri(self):
        """Test URI triple writing."""
        writer = NTriplesWriter()
        writer.write_uri('http://example.org/s', 'http://example.org/p', 'http://example.org/o')
        triples = writer.get_sorted_triples()
        assert len(triples) == 1
        assert triples[0] == '<http://example.org/s> <http://example.org/p> <http://example.org/o> .\n'

    def test_sorted_deterministic_output(self):
        """Test that output is sorted lexicographically for determinism."""
        writer = NTriplesWriter()
        # Add in reverse alphabetical order
        writer.write_lit('http://z.org/s', 'http://p.org/p', 'z')
        writer.write_lit('http://a.org/s', 'http://p.org/p', 'a')
        writer.write_lit('http://m.org/s', 'http://p.org/p', 'm')

        triples = writer.get_sorted_triples()
        assert len(triples) == 3
        # Should be sorted alphabetically
        assert triples[0].startswith('<http://a.org/s>')
        assert triples[1].startswith('<http://m.org/s>')
        assert triples[2].startswith('<http://z.org/s>')

    def test_write_to_file(self):
        """Test writing to file."""
        writer = NTriplesWriter()
        writer.write_lit('http://example.org/s', 'http://example.org/p', 'value')

        output = StringIO()
        writer.write_to_file(output)
        content = output.getvalue()
        assert '<http://example.org/s> <http://example.org/p> "value" .\n' in content


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

    def test_deterministic_output_identical_runs(self, tmp_path):
        """Test that data triples are deterministic (provenance timestamps vary)."""
        class TestEnricher(BaseEnricher):
            def _query_packages(self):
                return [('pkg2', 'http://example.org/pkg2'), ('pkg1', 'http://example.org/pkg1')]

            def _process_item(self, item):
                name, uri = item
                self.writer.write_lit(uri, 'http://example.org/name', name)

        output_file1 = tmp_path / 'run1.nt'
        output_file2 = tmp_path / 'run2.nt'
        mock_client = MagicMock()

        for output_file in [output_file1, output_file2]:
            enricher = TestEnricher(
                sparql_client=mock_client,
                output_path=str(output_file),
                enricher_name='test',
                enricher_version='1.0.0'
            )
            with patch.object(enricher, '_validate_fuseki_recency'):
                enricher.enrich()

        # Data triples should be deterministic (filter out timestamp-varying lines)
        def get_data_triples(file_path):
            content = file_path.read_text()
            lines = content.split('\n')
            # Only keep data triples (exclude provenance which has timestamps)
            data_lines = [
                line for line in lines
                if line.strip() and 'http://example.org/name' in line
            ]
            return data_lines

        data1 = get_data_triples(output_file1)
        data2 = get_data_triples(output_file2)

        # Data triples should be identical and sorted
        assert data1 == data2
        assert data1[0].split('"')[1] == 'pkg1'  # First is pkg1 (sorted)
        assert data1[1].split('"')[1] == 'pkg2'  # Second is pkg2

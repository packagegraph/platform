"""Tests for canned query CLI commands."""

import pytest
import json
from unittest.mock import patch, MagicMock
from click.testing import CliRunner
from packagegraph.cli import cli


def _mock_sparql_response(bindings):
    """Create a mock requests.post response with SPARQL results."""
    m = MagicMock()
    m.status_code = 200
    m.json.return_value = {"results": {"bindings": bindings}}
    m.raise_for_status.return_value = None
    return m


@pytest.mark.unit
class TestQueryCLI:
    def test_query_stats_command_exists(self):
        """Test that the query-stats command is registered."""
        runner = CliRunner()
        result = runner.invoke(cli, ["query-stats", "--help"])
        assert result.exit_code == 0
        assert "Distribution statistics" in result.output

    def test_query_search_command_exists(self):
        """Test that the query-search command is registered."""
        runner = CliRunner()
        result = runner.invoke(cli, ["query-search", "--help"])
        assert result.exit_code == 0
        assert "Search packages by name" in result.output

    def test_query_deps_command_exists(self):
        """Test that the query-deps command is registered."""
        runner = CliRunner()
        result = runner.invoke(cli, ["query-deps", "--help"])
        assert result.exit_code == 0

    def test_query_vulns_command_exists(self):
        """Test that the query-vulns command is registered."""
        runner = CliRunner()
        result = runner.invoke(cli, ["query-vulns", "--help"])
        assert result.exit_code == 0

    def test_query_stats_returns_json(self):
        """Test that query-stats returns JSON output."""
        runner = CliRunner()
        bindings = [
            {"distro": {"value": "debian"}, "packages": {"value": "50000"}, "versions": {"value": "60000"}}
        ]

        with patch("packagegraph.sparql_client.requests.post", return_value=_mock_sparql_response(bindings)):
            result = runner.invoke(cli, [
                "query-stats",
                "--fuseki-endpoint", "http://localhost:3030/packagegraph"
            ])

        assert result.exit_code == 0
        data = json.loads(result.output)
        assert len(data) == 1
        assert data[0]["distro"] == "debian"

    def test_query_search_with_package_name(self):
        """Test searching for a specific package."""
        runner = CliRunner()
        bindings = [
            {
                "name": {"value": "curl"},
                "version": {"value": "7.88.1"},
                "distro": {"value": "debian"},
            }
        ]

        with patch("packagegraph.sparql_client.requests.post", return_value=_mock_sparql_response(bindings)):
            result = runner.invoke(cli, [
                "query-search", "curl",
                "--fuseki-endpoint", "http://localhost:3030/packagegraph"
            ])

        assert result.exit_code == 0
        data = json.loads(result.output)
        assert data[0]["name"] == "curl"

    def test_query_deps_with_package_name(self):
        """Test querying dependencies of a package."""
        runner = CliRunner()
        bindings = [
            {"dep_name": {"value": "libc6"}, "dep_type": {"value": "depends"}}
        ]

        with patch("packagegraph.sparql_client.requests.post", return_value=_mock_sparql_response(bindings)):
            result = runner.invoke(cli, [
                "query-deps", "bash",
                "--fuseki-endpoint", "http://localhost:3030/packagegraph"
            ])

        assert result.exit_code == 0
        data = json.loads(result.output)
        assert data[0]["dep_name"] == "libc6"

    def test_query_vulns_returns_cves(self):
        """Test querying vulnerable packages."""
        runner = CliRunner()
        bindings = [
            {
                "pkg_name": {"value": "openssl"},
                "cve_id": {"value": "CVE-2022-0778"},
                "severity": {"value": "7.5"},
            }
        ]

        with patch("packagegraph.sparql_client.requests.post", return_value=_mock_sparql_response(bindings)):
            result = runner.invoke(cli, [
                "query-vulns",
                "--fuseki-endpoint", "http://localhost:3030/packagegraph"
            ])

        assert result.exit_code == 0
        data = json.loads(result.output)
        assert data[0]["cve_id"] == "CVE-2022-0778"

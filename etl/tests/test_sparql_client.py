from unittest.mock import patch, MagicMock
import pytest
from packagegraph.sparql_client import SparqlQueryClient


def _mock_response(json_data, status_code=200):
    mock = MagicMock()
    mock.status_code = status_code
    mock.json.return_value = json_data
    mock.raise_for_status.return_value = None
    return mock


@pytest.mark.unit
class TestSparqlQueryClient:
    def test_query_returns_bindings(self):
        client = SparqlQueryClient("http://fuseki:3030/packagegraph")
        data = {
            "results": {"bindings": [{"name": {"type": "literal", "value": "bash"}}]}
        }
        with patch(
            "packagegraph.sparql_client.requests.post",
            return_value=_mock_response(data),
        ):
            results = client.query("SELECT ?name WHERE { ?p pkg:packageName ?name }")
            assert len(results) == 1
            assert results[0]["name"]["value"] == "bash"

    def test_query_package_names_returns_tuples(self):
        client = SparqlQueryClient("http://fuseki:3030/packagegraph")
        data = {
            "results": {
                "bindings": [
                    {
                        "name": {"type": "literal", "value": "bash"},
                        "version": {"type": "literal", "value": "5.2"},
                    },
                ]
            }
        }
        with patch(
            "packagegraph.sparql_client.requests.post",
            return_value=_mock_response(data),
        ):
            results = client.query_package_names_and_versions()
            assert results == [("bash", "5.2")]

    def test_query_github_homepages_returns_tuples(self):
        client = SparqlQueryClient("http://fuseki:3030/packagegraph")
        data = {
            "results": {
                "bindings": [
                    {
                        "pkg": {"type": "uri", "value": "https://example.org/pkg1"},
                        "homepage": {
                            "type": "literal",
                            "value": "https://github.com/owner/repo",
                        },
                    },
                ]
            }
        }
        with patch(
            "packagegraph.sparql_client.requests.post",
            return_value=_mock_response(data),
        ):
            results = client.query_github_homepages()
            assert results == [
                ("https://example.org/pkg1", "https://github.com/owner/repo")
            ]

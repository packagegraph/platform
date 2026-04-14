"""Thin client for querying Fuseki SPARQL endpoint."""

import requests


class SparqlQueryClient:
    """Queries a Fuseki SPARQL endpoint and returns parsed results."""

    def __init__(self, endpoint: str):
        self.endpoint = endpoint.rstrip("/")
        self.sparql_url = f"{self.endpoint}/sparql"

    def query(self, sparql: str) -> list[dict]:
        """Execute a SPARQL query and return bindings."""
        response = requests.post(
            self.sparql_url,
            data={"query": sparql},
            headers={"Accept": "application/sparql-results+json"},
            timeout=120,
        )
        response.raise_for_status()
        return response.json()["results"]["bindings"]

    def query_package_names_and_versions(self) -> list[tuple[str, str]]:
        """Get unique (package_name, version_string) pairs."""
        sparql = """
        PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
        SELECT DISTINCT ?name ?version WHERE {
            ?p a pkg:BinaryPackage .
            ?p pkg:packageName ?name .
            ?p pkg:hasVersion ?v .
            ?v pkg:versionString ?version .
        }
        """
        bindings = self.query(sparql)
        return [(b["name"]["value"], b["version"]["value"]) for b in bindings]

    def query_github_homepages(self) -> list[tuple[str, str]]:
        """Get (package_uri, homepage_url) for packages with GitHub homepages."""
        sparql = """
        PREFIX pkg: <https://purl.org/packagegraph/ontology/core#>
        SELECT DISTINCT ?pkg ?homepage WHERE {
            ?pkg a pkg:BinaryPackage .
            ?pkg pkg:homepage ?homepage .
            FILTER(CONTAINS(STR(?homepage), "github.com"))
        }
        """
        bindings = self.query(sparql)
        return [(b["pkg"]["value"], b["homepage"]["value"]) for b in bindings]
